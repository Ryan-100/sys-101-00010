use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, dup2, execvp, fork, pipe, close, ForkResult, Pid};
use std::ffi::CString;
use std::io::{self, Write};
use std::env;
use std::os::unix::io::{RawFd, IntoRawFd};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;

enum Status {
    Continue,
    Exit,
}

#[derive(Debug, Clone)]
struct Command {
    commands: Vec<Vec<String>>,  
    input_file: Option<String>,   
    output_file: Option<String>,  
    background: bool,             
}

fn main() {
    loop {
        match process_next_line() {
            Ok(Status::Continue) => continue,
            Ok(Status::Exit) => break,
            Err(e) => eprintln!("Error: {}", e),
        }
    }
}

fn process_next_line() -> anyhow::Result<Status> {
    print_prompt()?;
    
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    
    if input.is_empty() {
        return Ok(Status::Continue);
    }
    
    if input == "exit" {
        return Ok(Status::Exit);
    }
    
    if input.starts_with("cd ") {
        let path = input[3..].trim();
        handle_cd(path)?;
        return Ok(Status::Continue);
    }
    
    if input == "cd" {
        if let Ok(home) = env::var("HOME") {
            handle_cd(&home)?;
        }
        return Ok(Status::Continue);
    }
    
    let command = parse_command(input)?;
    execute_command(&command)?;
    
    Ok(Status::Continue)
}

fn print_prompt() -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    print!("{}$ ", cwd.display());
    io::stdout().flush()?;
    Ok(())
}

fn handle_cd(path: &str) -> anyhow::Result<()> {
    chdir(path).map_err(|e| anyhow::anyhow!("cd failed: {}", e))?;
    Ok(())
}

fn parse_command(input: &str) -> anyhow::Result<Command> {
    let mut input = input.to_string();
    let mut background = false;
    let mut output_file = None;
    let mut input_file = None;
    
    if input.ends_with('&') {
        background = true;
        input = input[..input.len()-1].trim().to_string();
    }
    
    let mut pipeline: Vec<String> = input
        .split('|')
        .map(|s| s.trim().to_string())
        .collect();
    
    if let Some(last) = pipeline.last_mut() {
        if last.contains('>') {
            let last_clone = last.clone();
            let parts: Vec<&str> = last_clone.split('>').collect();
            if parts.len() == 2 {
                *last = parts[0].trim().to_string();
                output_file = Some(parts[1].trim().to_string());
            }
        }
    }
    
    if let Some(first) = pipeline.first_mut() {
        if first.contains('<') {
            let first_clone = first.clone();
            let parts: Vec<&str> = first_clone.split('<').collect();
            if parts.len() == 2 {
                *first = parts[0].trim().to_string();
                input_file = Some(parts[1].trim().to_string());
            }
        }
    }
    
    let commands: Vec<Vec<String>> = pipeline
        .iter()
        .map(|cmd| {
            cmd.split_whitespace()
                .map(|s| s.to_string())
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .collect();
    
    if commands.is_empty() {
        return Err(anyhow::anyhow!("Empty command"));
    }
    
    Ok(Command {
        commands,
        input_file,
        output_file,
        background,
    })
}

fn execute_command(command: &Command) -> anyhow::Result<()> {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            if command.background {
                println!("Starting background process {}", child);
            } else {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => {
                        if code != 0 {
                            eprintln!("Process exited with code {}", code);
                        }
                    }
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        eprintln!("Process killed by signal {:?}", signal);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("Wait failed: {}", e),
                }
            }
            Ok(())
        }
        Ok(ForkResult::Child) => {
            execute_pipeline(command);
            std::process::exit(1); 
        }
        Err(e) => Err(anyhow::anyhow!("Fork failed: {}", e)),
    }
}

fn execute_pipeline(command: &Command) {
    let num_commands = command.commands.len();
    
    if num_commands == 1 {
        execute_simple_command(command);
    } else {
        execute_pipeline_commands(command);
    }
}

fn execute_simple_command(command: &Command) {
    if let Some(ref input_file) = command.input_file {
        match open(
            input_file.as_str(),
            OFlag::O_RDONLY,
            Mode::empty()
        ) {
            Ok(fd) => {
                if let Err(e) = dup2(fd, 0) {
                    eprintln!("Failed to redirect input: {}", e);
                    std::process::exit(1);
                }
                let _ = close(fd);
            }
            Err(e) => {
                eprintln!("Failed to open input file '{}': {}", input_file, e);
                std::process::exit(1);
            }
        }
    }
    
    if let Some(ref output_file) = command.output_file {
        match open(
            output_file.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
            Mode::from_bits_truncate(0o644)
        ) {
            Ok(fd) => {
                if let Err(e) = dup2(fd, 1) {
                    eprintln!("Failed to redirect output: {}", e);
                    std::process::exit(1);
                }
                let _ = close(fd);
            }
            Err(e) => {
                eprintln!("Failed to open output file '{}': {}", output_file, e);
                std::process::exit(1);
            }
        }
    }
    
    let args = externalize(&command.commands[0]);
    if let Err(e) = execvp(&args[0], &args) {
        eprintln!("Failed to execute command: {}", e);
        std::process::exit(1);
    }
}

fn execute_pipeline_commands(command: &Command) {
    let num_commands = command.commands.len();
    let mut current_input: Option<RawFd> = None;
    
    for (i, cmd) in command.commands.iter().enumerate() {
        let is_last = i == num_commands - 1;
        
        let pipe_fds = if !is_last {
            match pipe() {
                Ok((read_fd, write_fd)) => {
                    let r: RawFd = read_fd.into_raw_fd();
                    let w: RawFd = write_fd.into_raw_fd();
                    Some((r, w))
                }
                Err(e) => {
                    eprintln!("Failed to create pipe: {}", e);
                    std::process::exit(1);
                }
            }
        } else {
            None
        };
        
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                if let Some(input_fd) = current_input {
                    if let Err(e) = dup2(input_fd, 0) {
                        eprintln!("Failed to redirect input: {}", e);
                        std::process::exit(1);
                    }
                    let _ = close(input_fd);
                } else if i == 0 {
                    if let Some(ref input_file) = command.input_file {
                        match open(
                            input_file.as_str(),
                            OFlag::O_RDONLY,
                            Mode::empty()
                        ) {
                            Ok(fd) => {
                                if let Err(e) = dup2(fd, 0) {
                                    eprintln!("Failed to redirect input: {}", e);
                                    std::process::exit(1);
                                }
                                let _ = close(fd);
                            }
                            Err(e) => {
                                eprintln!("Failed to open input file '{}': {}", input_file, e);
                                std::process::exit(1);
                            }
                        }
                    }
                }
                
                if let Some((read_fd, write_fd)) = pipe_fds {
                    let _ = close(read_fd);
                    if let Err(e) = dup2(write_fd, 1) {
                        eprintln!("Failed to redirect output: {}", e);
                        std::process::exit(1);
                    }
                    let _ = close(write_fd);
                } else if is_last {
                    if let Some(ref output_file) = command.output_file {
                        match open(
                            output_file.as_str(),
                            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
                            Mode::from_bits_truncate(0o644)
                        ) {
                            Ok(fd) => {
                                if let Err(e) = dup2(fd, 1) {
                                    eprintln!("Failed to redirect output: {}", e);
                                    std::process::exit(1);
                                }
                                let _ = close(fd);
                            }
                            Err(e) => {
                                eprintln!("Failed to open output file '{}': {}", output_file, e);
                                std::process::exit(1);
                            }
                        }
                    }
                }
                
                let args = externalize(cmd);
                if let Err(e) = execvp(&args[0], &args) {
                    eprintln!("Failed to execute command: {}", e);
                    std::process::exit(1);
                }
            }
            Ok(ForkResult::Parent { child: _ }) => {
                
                if let Some(input_fd) = current_input {
                    let _ = close(input_fd);
                }
                
                if let Some((read_fd, write_fd)) = pipe_fds {
                    let _ = close(write_fd);
                    current_input = Some(read_fd);
                }
            }
            Err(e) => {
                eprintln!("Fork failed: {}", e);
                std::process::exit(1);
            }
        }
    }
    
    if let Some(input_fd) = current_input {
        let _ = close(input_fd);
    }
    
    for _ in 0..num_commands {
        let _ = waitpid(Pid::from_raw(-1), None);
    }
    
    std::process::exit(0);
}

fn externalize(command: &[String]) -> Vec<CString> {
    command
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect()
}
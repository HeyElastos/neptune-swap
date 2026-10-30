use std::io::{self, BufRead, Write};

use neptune_provider::Provider;

fn main() {
    eprintln!("neptune-provider: starting v{}", env!("CARGO_PKG_VERSION"));
    let mut provider = Provider::from_env();
    let mut stdout = io::stdout();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (response, shutdown) = provider.handle_line(&line);
        writeln!(stdout, "{response}").and_then(|()| stdout.flush()).expect("stdout closed");
        if shutdown {
            break;
        }
    }
    eprintln!("neptune-provider: exiting");
}

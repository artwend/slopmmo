//! Client: windowed lightyear client with an Avian virtual character, plus a
//! headless CI mode.

mod app;
mod bindings;
mod character;
mod headless;
mod transport;

use std::net::SocketAddr;

use clap::Parser;

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    if args.headless {
        return headless::run(&args);
    }
    app::run(&args);
    std::process::ExitCode::SUCCESS
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "client",
    about = "Bevy window with an Avian virtual character, plus a headless CI mode",
    after_help = "window controls: WASD move, Space jump, Shift sprint"
)]
pub struct Args {
    /// run a scripted session, print metrics, exit
    #[arg(long)]
    pub headless: bool,
    /// safety cap on simulated seconds
    #[arg(long, value_name = "S", default_value_t = 30.0)]
    pub seconds: f32,
    /// send reports to a UDP server, e.g. 127.0.0.1:5888
    #[arg(long, value_name = "ADDR")]
    pub connect: Option<SocketAddr>,
}

mod app;
mod config;
mod input;
mod pty;
mod quad;
mod render;
mod term;

fn main() -> anyhow::Result<()> {
    app::run()
}

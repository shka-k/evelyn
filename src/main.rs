mod app;
mod color;
mod config;
mod input;
mod pty;
mod render;
mod term;

fn main() -> anyhow::Result<()> {
    app::run()
}

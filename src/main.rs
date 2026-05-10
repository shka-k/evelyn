mod app;
mod color;
mod config;
mod input;
mod pty;
mod render;
mod term;
mod themes;
mod width;

fn main() -> anyhow::Result<()> {
    app::run()
}

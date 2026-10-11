// GUI subsystem with no window of its own: Windower treats "has a main window"
// as "the game started", so the first window must be FFXI's.
#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(crystal_loader::run());
}

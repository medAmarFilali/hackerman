// No console window in release builds. The agent has none either: it's this same
// program, started again in the background.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let given = |wanted: &str| arguments.iter().any(|argument| argument == wanted);

    // The same program also does the jobs that make the lock screen work, none of which
    // have anything to do with the app: see crates/hk-win-service.
    #[cfg(windows)]
    {
        use hk_win_service as lock_screen;
        if given(lock_screen::SERVICE_ARGUMENT) {
            finish(lock_screen::run_service());
        } else if given(lock_screen::INSTALL_ARGUMENT) {
            finish(lock_screen::install_and_report(&arguments));
        } else if given(lock_screen::REMOVE_ARGUMENT) {
            finish(lock_screen::remove_and_report());
        } else if given(hackerman_app::AGENT_ARGUMENT) && given(lock_screen::HELPER_ARGUMENT) {
            finish(lock_screen::run_helper(&arguments));
        }
    }

    if given(hackerman_app::AGENT_ARGUMENT) {
        finish(hk_daemon::run_in_background());
    } else {
        hackerman_app::run();
    }
}

fn finish<E: std::fmt::Display>(result: Result<(), E>) -> ! {
    if let Err(error) = result {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn main() {
    // Reap zombie children automatically so launched agent processes don't
    // accumulate as zombies when they exit without the parent calling waitpid.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }

    if let Err(error) = trelane::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

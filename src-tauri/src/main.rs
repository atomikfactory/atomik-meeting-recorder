#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| arg == "--diarization-worker")
    {
        let code = match args.as_slice() {
            [_, request, result] => meeting_recorder_lib::run_diarization_worker(
                std::path::Path::new(request),
                std::path::Path::new(result),
            )
            .map(|_| 0)
            .unwrap_or(1),
            _ => 2,
        };
        std::process::exit(code);
    }
    meeting_recorder_lib::run();
}

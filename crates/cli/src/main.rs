use clap::{Parser, error::ErrorKind};
fn main() {
    let cli = match analytics_cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                print!("{error}");
                return;
            }
            let error = analytics_workflow::service::ServiceError::new("invalid_request", error);
            println!("{}", serde_json::json!({"error":error}));
            std::process::exit(2);
        }
    };
    let format = cli.format;
    match analytics_cli::execute(cli) {
        Ok(value) => match format {
            analytics_cli::Format::Json => println!("{}", serde_json::to_string(&value).unwrap()),
            analytics_cli::Format::Text => {
                println!("{}", serde_json::to_string_pretty(&value).unwrap())
            }
        },
        Err(error) => {
            let code = analytics_cli::exit_code(&error);
            eprintln!("{error}");
            println!("{}", serde_json::json!({"error":error}));
            std::process::exit(code);
        }
    }
}

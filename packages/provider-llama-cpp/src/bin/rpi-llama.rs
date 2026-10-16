#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match rpi_provider_llama_cpp::command::parse_llama_command(&args) {
        Ok(command) => rpi_provider_llama_cpp::command::run_llama_command(command).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

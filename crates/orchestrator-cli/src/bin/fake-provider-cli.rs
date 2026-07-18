fn main() {
    orchestrator_test_support::fake_cli_main(std::env::args_os().skip(1));
}

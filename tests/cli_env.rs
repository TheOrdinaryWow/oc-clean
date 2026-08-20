use clap::CommandFactory;
use oc_clean::cli::Cli;

const DESTRUCTIVE_FLAGS: [&str; 5] = [
    "apply",
    "force",
    "force-schema",
    "dangerously-skip-confirm",
    "skip-backup",
];

#[test]
fn environment_bindings_are_explicit_and_safe() {
    let command = Cli::command();

    for argument in command
        .get_arguments()
        .filter(|argument| argument.get_long().is_some() && argument.get_id().as_str() != "help")
    {
        let long = argument
            .get_long()
            .expect("filtered arguments must have a long name");

        if DESTRUCTIVE_FLAGS.contains(&long) {
            assert_eq!(
                argument.get_env(),
                None,
                "destructive flag --{long} must not accept an environment variable"
            );
            continue;
        }

        let environment = argument
            .get_env()
            .unwrap_or_else(|| panic!("option --{long} must declare an OCC_ environment variable"));
        assert!(
            environment.to_string_lossy().starts_with("OCC_"),
            "option --{long} has invalid environment variable {}",
            environment.to_string_lossy()
        );
    }
}

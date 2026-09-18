//! The JSON contract for `--format json`. Kept as an integration test because it is the shape
//! other tools consume, not an internal detail: an agent parses this, so a field appearing or
//! disappearing is a breaking change.

use ktsense_core::{Declaration, FileSkeleton, Parameter, Visibility};

fn sample() -> FileSkeleton {
    FileSkeleton::new("app/service/UserService.kt")
        .in_package("app.service")
        .with_declarations(vec![Declaration::class("UserService", 12)
            .with_parameters(vec![Parameter::new("repo", "UserRepository")
                .declaring_property(Visibility::Private, false)])
            .extending(vec!["Service".to_string()])
            .containing(vec![
                Declaration::function("createUser", 14).returning("Result<User>")
            ])])
}

#[test]
fn json_round_trips_and_omits_every_defaulted_field() {
    let json = serde_json::to_string(&sample()).expect("serialize");
    let restored: FileSkeleton = serde_json::from_str(&json).expect("deserialize");

    let observed = (
        restored == sample(),
        json.contains("\"visibility\":\"public\""),
        json.contains("\"modifiers\""),
        json.contains("\"imports\""),
        json.contains("\"return_type\":\"Result<User>\""),
        json.contains("\"property\":{\"visibility\":\"private\",\"mutable\":false}"),
    );

    assert_eq!(observed, (true, false, false, false, true, true));
}

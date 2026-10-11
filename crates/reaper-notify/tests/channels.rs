//! T091: channel files and URLs (FR-035, FR-036, FR-039).

use std::path::PathBuf;

use reaper_notify::{ChannelError, Provider, load_dir, parse_channel};

const TOKEN: &str = "s3cr3t-t0ken";

fn provider(url: &str) -> Result<Provider, ChannelError> {
    parse_channel("c", url).map(|c| c.provider())
}

#[test]
fn provider_is_inferred_from_the_url() {
    for (url, expected) in [
        ("https://discord.com/api/webhooks/1/abc", Provider::Discord),
        (
            "https://discordapp.com/api/webhooks/1/abc",
            Provider::Discord,
        ),
        (
            "https://ptb.discord.com/api/webhooks/1/abc",
            Provider::Discord,
        ),
        ("https://discord.com/not-a-webhook", Provider::Generic),
        ("https://hooks.slack.com/services/A/B/C", Provider::Slack),
        (
            "https://alerts.example.test/hooks/reaper",
            Provider::Generic,
        ),
        (
            "smtps://smtp.example.test?from=r@example.test&to=o@example.test",
            Provider::Email,
        ),
        (
            "smtp://smtp.example.test?from=r@example.test&to=o@example.test",
            Provider::Email,
        ),
    ] {
        assert_eq!(provider(url), Ok(expected), "{url}");
    }
}

#[test]
fn format_overrides_inference() {
    assert_eq!(
        provider("https://chat.example.test/hooks/x?format=slack"),
        Ok(Provider::Slack)
    );
    assert_eq!(
        provider("https://chat.example.test/hooks/x?format=discord"),
        Ok(Provider::Discord)
    );
    assert_eq!(
        provider("https://hooks.slack.com/services/A?format=generic"),
        Ok(Provider::Generic)
    );
    assert_eq!(
        provider("https://chat.example.test/x?format=teams"),
        Err(ChannelError::Format)
    );
    assert_eq!(
        provider("smtps://smtp.example.test?from=r@example.test&to=o@example.test&format=slack"),
        Err(ChannelError::FormatOnEmail)
    );
}

#[test]
fn unencrypted_transports_need_insecure() {
    assert_eq!(
        provider("http://echo.example.test/hook"),
        Err(ChannelError::Insecure("http"))
    );
    assert_eq!(
        provider("http://echo.example.test/hook?insecure=true"),
        Ok(Provider::Generic)
    );
    assert_eq!(provider("ftp://example.test/x"), Err(ChannelError::Scheme));
    assert_eq!(provider("not a url"), Err(ChannelError::NotAUrl));
    assert_eq!(provider("  \n"), Err(ChannelError::Empty));
}

#[test]
fn email_addresses_come_from_the_query() {
    let channel = parse_channel(
        "oncall-mail",
        "smtps://reaper%40example.test:pa%3Ass@smtp.example.test:465?from=reaper@example.test&to=oncall@example.test,%20sre@example.test\n",
    )
    .unwrap();
    assert_eq!(channel.email_from().as_deref(), Some("reaper@example.test"));
    assert_eq!(
        channel.email_to(),
        ["oncall@example.test", "sre@example.test"]
    );

    let err = |url| parse_channel("m", url).unwrap_err();
    assert_eq!(
        err("smtps://smtp.example.test?to=o@example.test"),
        ChannelError::NoFrom
    );
    assert_eq!(
        err("smtps://smtp.example.test?from=r@example.test"),
        ChannelError::NoTo
    );
    assert_eq!(
        err("smtps://smtp.example.test?from=r@example.test&to=,"),
        ChannelError::NoTo
    );
    assert_eq!(
        err("smtps://smtp.example.test?from=nope&to=o@example.test"),
        ChannelError::Address("from")
    );
    assert_eq!(
        err("smtps://smtp.example.test?from=r@example.test&to=o@example.test,bad"),
        ChannelError::Address("to")
    );
}

#[test]
fn nothing_secret_reaches_debug_or_errors() {
    let urls = [
        format!("https://discord.com/api/webhooks/1/{TOKEN}"),
        format!("https://alerts.example.test/hook?token={TOKEN}"),
        format!("smtps://user:{TOKEN}@smtp.example.test?from=r@example.test&to=o@example.test"),
    ];
    for url in &urls {
        let channel = parse_channel("leak-check", url).unwrap();
        let shown = format!("{channel:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("leak-check"));
    }
    let bad = [
        format!("http://alerts.example.test/{TOKEN}"),
        format!("https://alerts.example.test/{TOKEN}?format={TOKEN}"),
        format!("{TOKEN}://x"),
        format!("smtps://user:{TOKEN}@smtp.example.test?from={TOKEN}&to=o@example.test"),
        format!("https://[{TOKEN}"),
    ];
    for url in &bad {
        let err = parse_channel("leak-check", url).unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("channels-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_directory_is_read_like_a_mounted_secret() {
    let dir = scratch("mounted");
    // What the kubelet writes: a hidden data dir, links to it, and keys.
    std::fs::create_dir(dir.join("..2026_10_10")).unwrap();
    std::fs::write(
        dir.join("..2026_10_10/ops-discord"),
        "https://discord.com/api/webhooks/1/x\n",
    )
    .unwrap();
    std::fs::write(dir.join("..data"), "ignored").unwrap();
    std::fs::write(dir.join(".hidden"), "https://alerts.example.test/x").unwrap();
    std::fs::write(
        dir.join("team-slack"),
        "https://hooks.slack.com/services/A/B/C",
    )
    .unwrap();
    std::fs::write(dir.join("broken"), "http://plain.example.test/x").unwrap();
    std::fs::create_dir(dir.join("subdir")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("..2026_10_10/ops-discord", dir.join("ops-discord")).unwrap();

    let set = load_dir(&dir);
    let valid: Vec<(&str, Provider)> = set.valid.iter().map(|c| (c.name(), c.provider())).collect();
    #[cfg(unix)]
    assert_eq!(
        valid,
        [
            ("ops-discord", Provider::Discord),
            ("team-slack", Provider::Slack)
        ]
    );
    assert_eq!(
        set.invalid,
        [("broken".to_owned(), ChannelError::Insecure("http"))]
    );
}

#[test]
fn a_missing_directory_means_no_channels() {
    let set = load_dir(&PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("does-not-exist"));
    assert_eq!(set.valid.len() + set.invalid.len(), 0);
}

//! Adopt only descriptors explicitly transferred by a desktop supervisor.
//! Tokio-owned nonblocking pipes/socket pairs make pending ingress cancellable.
/// Resolve the source host-power intervals from modern or legacy profiles.
pub fn options_for_settings(
    mode: &str,
    settings: &t3_contracts::ServerSettings,
) -> crate::desktop_telemetry::DesktopTelemetryOptions {
    use t3_contracts::{BackgroundActivityProfile as P, BackgroundActivityProfileSelection as S};
    let background = &settings.background_activity;
    let empty = serde_json::to_value(&background.overrides)
        .unwrap()
        .as_object()
        .unwrap()
        .is_empty();
    let profile = if background.profile == S::Balanced && background.base_profile.is_none() && empty
    {
        settings.background_activity_profile
    } else {
        match background.profile {
            S::Performance => P::Performance,
            S::BatterySaver => P::BatterySaver,
            S::Balanced => P::Balanced,
            S::Custom => background.base_profile.unwrap_or(P::Balanced),
        }
    };
    let (active, idle) = match profile {
        P::Performance => (30_000, 120_000),
        P::Balanced => (30_000, 300_000),
        P::BatterySaver => (60_000, 600_000),
    };
    let mut options = crate::desktop_telemetry::DesktopTelemetryOptions::unavailable(mode);
    let millis = |value: &serde_json::Number| {
        let value = value.as_f64().unwrap();
        (value.floor() + if value - value.floor() >= 0.5 { 1. } else { 0. }).max(1.) as u64
    };
    options.active_interval_ms = if background.profile == S::Custom {
        background
            .overrides
            .host_power_monitor_active_interval
            .as_ref()
            .map(millis)
            .unwrap_or(active)
    } else {
        active
    };
    options.idle_interval_ms = if background.profile == S::Custom {
        background
            .overrides
            .host_power_monitor_idle_interval
            .as_ref()
            .map(millis)
            .unwrap_or(idle)
    } else {
        idle
    };
    options
}
#[cfg(unix)]
use std::{
    io,
    os::fd::{AsRawFd, OwnedFd},
};
#[cfg(unix)]
fn is_socket(fd: &OwnedFd) -> io::Result<bool> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((unsafe { stat.assume_init() }.st_mode & libc::S_IFMT) == libc::S_IFSOCK)
}
#[cfg(unix)]
pub fn from_owned_descriptors(
    mut options: crate::desktop_telemetry::DesktopTelemetryOptions,
    input: Option<OwnedFd>,
    control: Option<OwnedFd>,
) -> io::Result<crate::desktop_telemetry::DesktopTelemetryOptions> {
    if let Some(fd) = input {
        let number = fd.as_raw_fd();
        let reader: crate::desktop_telemetry::Reader = if is_socket(&fd)? {
            let stream = std::os::unix::net::UnixStream::from(fd);
            stream.set_nonblocking(true)?;
            Box::pin(tokio::net::UnixStream::from_std(stream)?)
        } else {
            Box::pin(tokio::net::unix::pipe::Receiver::from_owned_fd(fd)?)
        };
        options.input = Some((number, reader));
    }
    if let Some(fd) = control {
        let number = fd.as_raw_fd();
        let writer: crate::desktop_telemetry::Writer = if is_socket(&fd)? {
            let stream = std::os::unix::net::UnixStream::from(fd);
            stream.set_nonblocking(true)?;
            Box::pin(tokio::net::UnixStream::from_std(stream)?)
        } else {
            Box::pin(tokio::net::unix::pipe::Sender::from_owned_fd(fd)?)
        };
        options.control = Some((number, writer));
    }
    Ok(options)
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    #[tokio::test]
    async fn actual_pipe_and_spawn_style_socketpair_are_owned_and_shutdown_cancels_ingress() {
        for socket in [false, true] {
            let (input, mut source): (OwnedFd, crate::desktop_telemetry::Writer) = if socket {
                let (server, host) = std::os::unix::net::UnixStream::pair().unwrap();
                host.set_nonblocking(true).unwrap();
                (
                    server.into(),
                    Box::pin(tokio::net::UnixStream::from_std(host).unwrap()),
                )
            } else {
                let (sender, receiver) = tokio::net::unix::pipe::pipe().unwrap();
                (receiver.into_blocking_fd().unwrap(), Box::pin(sender))
            };
            let (control, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            let mut reader = BufReader::new(tokio::net::UnixStream::from_std(reader).unwrap());
            let options = from_owned_descriptors(
                crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("desktop"),
                Some(input),
                Some(control.into()),
            )
            .unwrap();
            let receiver = crate::desktop_telemetry::DesktopTelemetryReceiver::new(options).await;
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains("setHostPowerIntervals"));
            let mut health = receiver.subscribe_health();
            source
                .write_all(
                    b"{\"version\":1,\"type\":\"desktopTelemetryHello\",\"electronPid\":100}\n",
                )
                .await
                .unwrap();
            assert_eq!(
                health.recv().await.unwrap().status,
                t3_contracts::ResourceTelemetrySourceStatus::Healthy
            );
            receiver.shutdown().await;
            let mut byte = [0];
            assert_eq!(reader.read(&mut byte).await.unwrap(), 0);
            assert!(source.write_all(b"closed").await.is_err());
        }
    }
}
#[cfg(test)]
mod settings_tests {
    #[test]
    fn resolved_host_power_intervals_match_original_modern_and_legacy_settings() {
        for (index, line) in include_str!("../tests/fixtures/desktop-telemetry-settings.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: serde_json::Value = serde_json::from_str(line).unwrap();
            let settings: t3_contracts::ServerSettings =
                serde_json::from_value(fixture["input"].clone()).unwrap();
            let options = super::options_for_settings("desktop", &settings);
            assert_eq!(
                options.active_interval_ms,
                fixture["active"].as_u64().unwrap(),
                "source active interval witness {index}"
            );
            assert_eq!(
                options.idle_interval_ms,
                fixture["idle"].as_u64().unwrap(),
                "source idle interval witness {index}"
            );
        }
    }
}

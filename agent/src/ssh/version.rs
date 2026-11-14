use std::time::Duration;

/// The host-key observer needs KnownHostsCommand, introduced in OpenSSH 8.5.
pub fn check_openssh() -> Result<(u32, u32), String> {
    let output = super::run(std::process::Command::new("ssh").arg("-V"), Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    if !output.status.success() || output.timed_out {
        return Err(output.last_error());
    }
    parse_version(&String::from_utf8_lossy(&output.stderr))
}

fn parse_version(text: &str) -> Result<(u32, u32), String> {
    let version = text
        .split_whitespace()
        .find_map(|word| word.strip_prefix("OpenSSH_"))
        .ok_or_else(|| "could not read the OpenSSH version from ssh -V".to_owned())?;
    let (major, rest) = version
        .split_once('.')
        .ok_or_else(|| "could not read the OpenSSH version from ssh -V".to_owned())?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let version = major.parse::<u32>().ok().zip(minor.parse::<u32>().ok());
    let (major, minor) = version.ok_or_else(|| "could not read the OpenSSH version from ssh -V".to_owned())?;
    if (major, minor) < (8, 5) {
        return Err(format!(
            "remote projects need OpenSSH 8.5 or newer (found {major}.{minor})"
        ));
    }
    Ok((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_older_than_8_5_is_refused_with_the_version() {
        assert_eq!(
            parse_version("OpenSSH_8.4p1, OpenSSL").unwrap_err(),
            "remote projects need OpenSSH 8.5 or newer (found 8.4)"
        );
        assert_eq!(parse_version("OpenSSH_10.0p2, OpenSSL").unwrap(), (10, 0));
        assert_eq!(parse_version("OpenSSH_8.5p1, OpenSSL").unwrap(), (8, 5));
        assert!(parse_version("other ssh 10.0").is_err());
        assert!(parse_version("OpenSSH_8.x").is_err());
    }
}

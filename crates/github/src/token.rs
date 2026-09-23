use std::{fmt, process::Command};

use color_eyre::{
    Section,
    eyre::{Result, WrapErr, eyre},
};

/// A GitHub token. `Debug` and `Display` never show the value.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// `$GITHUB_TOKEN` if set, otherwise `gh auth token`.
    pub fn discover() -> Result<Self> {
        if let Ok(value) = std::env::var("GITHUB_TOKEN")
            && !value.trim().is_empty()
        {
            return Ok(Self(value.trim().to_owned()));
        }
        let output = Command::new("gh")
            .args(["auth", "token"])
            .output()
            .wrap_err("running `gh auth token`")
            .suggestion("install the GitHub CLI, or set GITHUB_TOKEN")?;
        let value = String::from_utf8(output.stdout)?.trim().to_owned();
        if !output.status.success() || value.is_empty() {
            return Err(eyre!("`gh auth token` returned no token"))
                .suggestion("run `gh auth login`, or set GITHUB_TOKEN");
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_value() {
        let token = Token::new("ghp_secret".into());
        assert!(!format!("{token:?}").contains("secret"));
    }
}

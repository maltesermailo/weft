//! IRC message codec (RFC 2812 §2.3.1): `[':' prefix SP] command params
//! [SP ':' trailing]`. Lenient-in (tolerate extra spaces, missing pieces),
//! strict-out. The trailing is folded into `params` as the final element on
//! parse and re-emitted with its `:` marker on format when it needs one.

/// A parsed IRC line. `command` is upper-cased (IRC verbs are
/// case-insensitive); numeric replies keep their three digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

impl Message {
    /// First param or `""` — convenient for the many one-arg IRC verbs.
    pub fn arg(&self, i: usize) -> &str {
        self.params.get(i).map(String::as_str).unwrap_or("")
    }
}

/// Parse one IRC line (without its CRLF). Returns `None` for a blank line.
pub fn parse(line: &str) -> Option<Message> {
    let mut rest = line.trim_end_matches(['\r', '\n']).trim_start();
    let mut prefix = None;
    if let Some(after) = rest.strip_prefix(':') {
        let (p, r) = after.split_once(' ')?;
        prefix = Some(p.to_string());
        rest = r.trim_start();
    }
    let (command, mut rest) = match rest.split_once(' ') {
        Some((c, r)) => (c, r.trim_start()),
        None => (rest, ""),
    };
    if command.is_empty() {
        return None;
    }
    let mut params = Vec::new();
    while !rest.is_empty() {
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing.to_string());
            break;
        }
        match rest.split_once(' ') {
            Some((p, r)) => {
                params.push(p.to_string());
                rest = r.trim_start();
            }
            None => {
                params.push(rest.to_string());
                break;
            }
        }
    }
    Some(Message {
        prefix,
        command: command.to_ascii_uppercase(),
        params,
    })
}

/// Format a server→client line (no CRLF; the writer adds it). The final param
/// is emitted as a `:trailing` when it is empty, contains a space, or starts
/// with `:` — exactly the cases the parser would otherwise mis-split.
pub fn format(prefix: Option<&str>, command: &str, params: &[&str]) -> String {
    let mut out = String::new();
    if let Some(prefix) = prefix {
        out.push(':');
        out.push_str(prefix);
        out.push(' ');
    }
    out.push_str(command);
    for (i, param) in params.iter().enumerate() {
        out.push(' ');
        let last = i + 1 == params.len();
        if last && (param.is_empty() || param.contains(' ') || param.starts_with(':')) {
            out.push(':');
        }
        out.push_str(param);
    }
    out
}

/// Format a line whose final field is a **trailing** — always emitted with
/// the `:` marker (PRIVMSG/NOTICE bodies, numeric text): the IRC convention
/// even when the text has no space.
pub fn format_msg(prefix: Option<&str>, command: &str, middle: &[&str], trailing: &str) -> String {
    let mut out = String::new();
    if let Some(prefix) = prefix {
        out.push(':');
        out.push_str(prefix);
        out.push(' ');
    }
    out.push_str(command);
    for m in middle {
        out.push(' ');
        out.push_str(m);
    }
    out.push(' ');
    out.push(':');
    out.push_str(trailing);
    out
}

/// Numeric replies used by the gateway (RFC 2812 subset).
pub mod rpl {
    pub const WELCOME: &str = "001";
    pub const YOURHOST: &str = "002";
    pub const CREATED: &str = "003";
    pub const MYINFO: &str = "004";
    pub const ISUPPORT: &str = "005";
    pub const LIST: &str = "322";
    pub const LISTEND: &str = "323";
    pub const NAMREPLY: &str = "353";
    pub const ENDOFNAMES: &str = "366";
    pub const MOTDSTART: &str = "375";
    pub const MOTD: &str = "372";
    pub const ENDOFMOTD: &str = "376";
}

/// Error numerics.
pub mod err {
    pub const NOSUCHCHANNEL: &str = "403";
    pub const NONICKNAMEGIVEN: &str = "431";
    pub const ERRONEUSNICKNAME: &str = "432";
    pub const NOTREGISTERED: &str = "451";
    pub const PASSWDMISMATCH: &str = "464";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefix_command_params_trailing() {
        let m = parse(":nick!u@h PRIVMSG #chan :hello there\r\n").unwrap();
        assert_eq!(m.prefix.as_deref(), Some("nick!u@h"));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#chan", "hello there"]);
    }

    #[test]
    fn parses_no_prefix_and_uppercases_command() {
        let m = parse("join #general").unwrap();
        assert_eq!(m.prefix, None);
        assert_eq!(m.command, "JOIN");
        assert_eq!(m.arg(0), "#general");
    }

    #[test]
    fn trailing_may_be_empty_or_contain_colons() {
        let m = parse("PRIVMSG #c ::-)").unwrap();
        assert_eq!(m.params, vec!["#c", ":-)"]);
        let blank = parse("PRIVMSG #c :").unwrap();
        assert_eq!(blank.params, vec!["#c", ""]);
    }

    #[test]
    fn blank_line_is_none() {
        assert!(parse("   \r\n").is_none());
    }

    #[test]
    fn format_adds_trailing_marker_only_when_needed() {
        assert_eq!(
            format(
                Some("weft.example"),
                rpl::WELCOME,
                &["ada", "Welcome to WEFT"]
            ),
            ":weft.example 001 ada :Welcome to WEFT"
        );
        // A single spaceless final param stays bare.
        assert_eq!(format(None, "JOIN", &["#general"]), "JOIN #general");
        // Namespaced channels round-trip (the `/` is a legal chanstring char).
        let m = parse(&format(None, "JOIN", &["#gaming/general"])).unwrap();
        assert_eq!(m.arg(0), "#gaming/general");
    }
}

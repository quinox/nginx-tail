use std::fmt::Display;

use crate::terminal::colors;

#[derive(PartialEq, Debug)]
pub struct ParsedLine {
    // <field>            the field itself
    // <field1>_<field2>  the data between field1 and field2
    head: String,
    head_date: Option<String>,
    date: String,
    date_method: Option<String>,
    method: String,
    method_url: Option<String>,
    url: String,
    url_lvl: Option<String>,
    protocollvl: String,
    lvl_statuscode: Option<String>,
    statuscode: String,
    tail: String,
}

pub fn parse_nginx_line(line: &str) -> ParsedLine {
    // Has to be able to parse a partial line!
    // Take special consideration whether you've seen separator symbols:
    let mut head = "".to_owned();
    let mut head_date = None;
    let mut date = "".to_owned();
    let mut date_method = None;
    let mut method = "".to_owned();
    let mut method_url = None;
    let mut url = "".to_owned();
    let mut url_lvl = None;
    let mut protocollvl = "".to_owned();
    let mut lvl_statuscode = None;
    let mut statuscode = "".to_owned();
    let mut tail = "".to_owned();

    // om nom nom
    let mut chars = line.chars();
    #[allow(clippy::never_loop)]
    'outer: loop {
        loop {
            match chars.next() {
                None => break 'outer,
                Some('[') => break,
                Some(chr) => head.push(chr),
            }
        }
        head_date = Some("[".to_owned());

        loop {
            match chars.next() {
                None => break 'outer,
                Some(']') => break,
                Some(chr) => date.push(chr),
            }
        }
        date_method = Some("]".to_owned());
        match chars.next() {
            Some(' ') => date_method.as_mut().unwrap().push(' '),
            Some(x) => {
                tail.push(x);
                break 'outer;
            }
            None => break 'outer,
        }

        loop {
            match chars.next() {
                None => break 'outer,
                Some('"') => break,
                Some(chr) => date_method.as_mut().unwrap().push(chr),
            }
        }
        date_method.as_mut().unwrap().push('"');

        loop {
            match chars.next() {
                None => break 'outer,
                Some(' ') => break,
                Some(chr) => method.push(chr),
            }
        }
        method_url = Some(" ".to_owned());
        loop {
            match chars.next() {
                None => break 'outer,
                Some(' ') => break,
                Some(chr) => url.push(chr),
            }
        }
        url_lvl = Some(" ".to_owned());
        loop {
            match chars.next() {
                None => break 'outer,
                Some('"') => break,
                Some(chr) => protocollvl.push(chr),
            }
        }
        lvl_statuscode = Some("\"".to_owned());
        match chars.next() {
            Some(' ') => lvl_statuscode.as_mut().unwrap().push(' '),
            Some(x) => {
                tail.push(x);
                break 'outer;
            }
            None => break 'outer,
        }
        loop {
            match chars.next() {
                None => break 'outer,
                Some(' ') => {
                    tail.push(' ');
                    break;
                }
                Some(chr) => statuscode.push(chr),
            }
        }
        break 'outer; // who said Rust didn't have goto ;-)
    }
    tail.extend(chars);
    ParsedLine {
        head,
        head_date,
        date,
        date_method,
        method,
        method_url,
        url,
        url_lvl,
        protocollvl,
        lvl_statuscode,
        statuscode,
        tail,
    }
}

type ColorStartEnd = (&'static str, &'static str);

#[inline]
pub fn code2color(code: &str) -> ColorStartEnd {
    match code.chars().next() {
        None => ("", ""),
        Some('2') => (colors::GREEN, colors::RESET),
        Some('3') => (colors::PURPLE, colors::RESET),
        Some('4') => (colors::YELLOW, colors::RESET),
        Some('5') => (colors::RED, colors::RESET),
        _ => (colors::WHITE, colors::RESET),
    }
}

macro_rules! unwrap_or_print_tail_then_return_ok {
    ($f:expr, $var:expr, $tail:expr) => {
        if let Some(unwrapped) = $var {
            unwrapped
        } else {
            let _ = write!($f, "{}", $tail);
            return Ok(());
        }
    };
}

impl Display for ParsedLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Write all the bits separately, returning early when we run out of bits to print
        // self.tail will contain any unparsed text, don't forget to print it
        write!(f, "{}", &self.head)?;

        let head_date = unwrap_or_print_tail_then_return_ok!(f, &self.head_date, &self.tail);
        write!(f, "{head_date}{}", &self.date)?;

        let date_method = unwrap_or_print_tail_then_return_ok!(f, &self.date_method, &self.tail);
        let (color, reset) = match self.method.as_str() {
            "POST" => (colors::WHITE, colors::RESET),
            _ => ("", ""),
        };
        write!(f, "{date_method}{color}{}{reset}", &self.method)?;

        let method_url = unwrap_or_print_tail_then_return_ok!(f, &self.method_url, &self.tail);
        write!(f, "{method_url}{}", &self.url)?;

        let url_lvl = unwrap_or_print_tail_then_return_ok!(f, &self.url_lvl, &self.tail);
        write!(f, "{url_lvl}{}", &self.protocollvl)?;

        let lvl_statuscode =
            unwrap_or_print_tail_then_return_ok!(f, &self.lvl_statuscode, self.tail);

        let (color, reset) = code2color(&self.statuscode);
        write!(
            f,
            "{lvl_statuscode}{color}{}{reset}{}",
            &self.statuscode, &self.tail,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::terminal::colors::{GREEN, RESET};
    use crate::{
        extract_statuscode,
        parsing::{ParsedLine, parse_nginx_line},
    };

    #[test]
    fn test_parsing() {
        let variant1 = r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#.to_owned();
        assert_eq!("200", extract_statuscode(&variant1).unwrap());
        let variant2 = r#"123.123.123.123 - - [26/May/2025:19:43:59 +0200] "GET /links.json HTTP/1.1" 200 91 "-" "Monit/5.34.3" 0.004 0.004 ."#.to_owned();
        assert_eq!("200", extract_statuscode(&variant2).unwrap());

        // Deconstructing the struct because it looks nicer with assert_eq
        let ParsedLine {
            head,
            head_date,
            date,
            date_method,
            method,
            method_url,
            url,
            url_lvl,
            protocollvl,
            lvl_statuscode,
            statuscode,
            tail,
        } = parse_nginx_line(&variant2);
        assert_eq!(head, "123.123.123.123 - - ");
        assert_eq!(head_date.unwrap(), "[");
        assert_eq!(date, "26/May/2025:19:43:59 +0200");
        assert_eq!(date_method.unwrap(), "] \"");
        assert_eq!(method, "GET");
        assert_eq!(method_url.unwrap(), " ");
        assert_eq!(url, "/links.json");
        assert_eq!(url_lvl.unwrap(), " ");
        assert_eq!(protocollvl, "HTTP/1.1");
        assert_eq!(lvl_statuscode.unwrap(), "\" ");
        assert_eq!(statuscode, "200");
        assert_eq!(tail, r#" 91 "-" "Monit/5.34.3" 0.004 0.004 ."#);
    }
    #[test]
    fn test_formatting_v3() {
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v3 1.22.3.44 - - [26/May/2025:00:00:01 +0200] 1a2b3c4d5e6f "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#
                )
            ),
            format!(
                r#"v3 1.22.3.44 - - [26/May/2025:00:00:01 +0200] 1a2b3c4d5e6f "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" {GREEN}200{RESET} 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#
            ),
        );
    }

    #[test]
    fn test_formatting_corruption() {
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"vx 1.22.3.44 - - [26/May/2025:00:00:01 +0200]"GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#
                )
            ),
            format!(
                r#"vx 1.22.3.44 - - [26/May/2025:00:00:01 +0200]"GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#
            ),
        );
    }

    #[test]
    fn test_formatting_v2() {
        // every subsequent assert_eq cuts down the string to test boundary behavior
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#
                )
            ),
            format!(
                r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" {GREEN}200{RESET} 63 - 0.023 0.022 "-" "UserAgent/123" "https" "some.domain.example""#
            ),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 200 "#
                )
            ),
            format!(
                r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" {GREEN}200{RESET} "#
            ),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 20"#
                )
            ),
            format!(
                r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" {GREEN}20{RESET}"#
            ),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" 2"#
                )
            ),
            format!(
                r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" {GREEN}2{RESET}"#
            ),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" "#
                )
            ),
            format!(
                r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0" "#
            ),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(
                    r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0""#
                )
            ),
            format!(
                r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "GET /v2/installations/74453/stats?interval=hours&type=evcs&start=1748210400 HTTP/1.0""#
            ),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "G"#)
            ),
            format!(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "G"#),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] ""#)
            ),
            format!(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] ""#),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "#)
            ),
            format!(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200] "#),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200]"#)
            ),
            format!(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200]"#),
        );
        assert_eq!(
            format!(
                "{}",
                parse_nginx_line(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200"#)
            ),
            format!(r#"v2 1.22.3.44 - - [26/May/2025:00:00:01 +0200"#),
        );
    }
}

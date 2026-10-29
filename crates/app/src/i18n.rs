//! Dashboard text in English and Korean. Core messages stay English (the report uses them);
//! `phrase` translates the ones the dashboard shows: cleanup evidence and refusals.

use agentlens_core::rules::Language;

pub fn t(lang: Language, en: &'static str, ko: &'static str) -> &'static str {
    match lang {
        Language::En => en,
        Language::Ko => ko,
    }
}

/// `format!` in the current language; both strings take the same arguments.
macro_rules! tf {
    ($lang:expr, $en:literal, $ko:literal $(, $arg:expr)* $(,)?) => {
        match $lang {
            agentlens_core::rules::Language::En => format!($en $(, $arg)*),
            agentlens_core::rules::Language::Ko => format!($ko $(, $arg)*),
        }
    };
}
pub(crate) use tf;

/// Whole evidence tokens.
const WHOLE: &[(&str, &str)] = &[
    ("parent-gone", "부모 종료됨"),
    ("parent-pid-reused", "부모 PID 재사용"),
    ("adopted-by-init", "init에 입양"),
    ("started-by-agent", "에이전트가 실행"),
    ("not-agent", "에이전트 아님"),
    ("exposed", "외부 노출"),
    ("owner-gone", "주인 세션 종료"),
    ("agent-infra", "에이전트 기반 프로세스"),
    ("agent-command", "에이전트가 실행한 명령"),
    ("has-window", "창 있음"),
];

/// Evidence with a value after the word.
const PREFIX: &[(&str, &str)] = &[
    ("age ", "경과 "),
    ("idle ", "유휴 "),
    ("from tab ", "출처 탭 "),
    ("pids ", "PID "),
    ("listening ", "포트 열림 "),
    ("from session ", "출처 세션 "),
    ("in-use by ", "사용 중: "),
];

/// Refusals: `<name> (<pid>)` followed by the reason.
const REFUSAL: &[(&str, &str)] = &[
    (" runs agentlens itself", ": agentlens 자신을 실행 중"),
    (" is a host application", ": 호스트 앱 본체"),
    (" is a system process", ": 시스템 프로세스"),
    (" is protected by rule ", ": 보호 규칙 "),
];

pub fn phrase(lang: Language, text: &str) -> String {
    if lang == Language::En {
        return text.to_string();
    }
    if let Some((_, ko)) = WHOLE.iter().find(|(en, _)| *en == text) {
        return ko.to_string();
    }
    if let Some((rest, ko)) = PREFIX.iter().find_map(|(en, ko)| Some((text.strip_prefix(en)?, ko))) {
        return format!("{ko}{rest}");
    }
    if let Some(n) = text.strip_suffix(" sessions") {
        return format!("세션 {n}개");
    }
    if let Some((head, tail, ko)) = REFUSAL
        .iter()
        .find_map(|(en, ko)| text.split_once(en).map(|(h, t)| (h, t, ko)))
    {
        return format!("{head}{ko}{tail}");
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_translate_only_their_own_words() {
        let ko = |s| phrase(Language::Ko, s);
        assert_eq!(ko("parent-gone"), "부모 종료됨");
        // The tab label keeps its own words.
        assert_eq!(ko("from tab my-page/idle "), "출처 탭 my-page/idle ");
        assert_eq!(ko("2 sessions"), "세션 2개");
        assert_eq!(
            ko("Orca.exe (12) is a host application"),
            "Orca.exe (12): 호스트 앱 본체"
        );
        assert_eq!(phrase(Language::En, "parent-gone"), "parent-gone");
    }
}

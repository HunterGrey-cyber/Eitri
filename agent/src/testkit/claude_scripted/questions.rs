//! What the model is told about a C20 row's question once the CLI has the person's answer, checked the same way at
//! both layers against the request the stand-in matched after the answer. The records of one run can all agree
//! with each other and still carry an option nobody picked, so the text the model got is checked against the row's
//! own answer, not only against the other records.

use serde_json::Value;

/// The question call every C20 row scripts.
pub const QUESTION_CALL: &str = "toolu_q1";

/// What one row's answered question reaches the model as.
#[derive(Clone, Copy, Debug)]
pub struct Told {
    /// The scripted step whose request carries the question's result.
    pub step: &'static str,
    /// Whether that result is an error result; `None` leaves it open.
    pub is_error: Option<bool>,
    /// Per question: the question's text and the texts its own answer holds. Each question's
    /// answer is read from the question's text up to the next question's, so an answer given under the wrong
    /// question fails even when every text is somewhere in the result.
    pub answers: &'static [(&'static str, &'static [&'static str])],
    /// Texts it must not hold: options nobody picked.
    pub lacks: &'static [&'static str],
    /// Whether the request must carry a result for the call at all; an interrupted call may reach the model
    /// with none.
    pub required: bool,
}

const PAIR: Told = Told {
    step: "m2",
    is_error: Some(false),
    answers: &[
        ("Which colour should the C20 banner use?", &["Blue"]),
        ("Which C20 checks should run?", &["Lint, Fuzz"]),
    ],
    lacks: &["Green", "Unit", "Bench"],
    required: true,
};

/// The row's expectation, for the rows whose question reaches the model with a result.
pub fn told(id: &str) -> Option<Told> {
    Some(match id {
        "c20" | "c20_auto" | "c20_manual" | "c20x" => PAIR,
        "c20c" => Told {
            step: "m2",
            is_error: Some(true),
            answers: &[
                ("Which colour should the C20 banner use?", &["Answer: Blue"]),
                ("Which C20 checks should run?", &["(No answer provided)"]),
            ],
            lacks: &[],
            required: true,
        },
        "c20d" => Told {
            step: "m2",
            is_error: Some(false),
            answers: &[(
                "Which C20D cache notes apply?",
                &[r#""say \"hi\"""#, r#""Redis, not Memcached""#],
            )],
            lacks: &["Plain", "Mem\u{202e}cached"],
            required: true,
        },
        "c20e" => Told {
            step: "m2",
            is_error: Some(false),
            answers: &[(
                "Which C20E layout reads better?",
                &["List", "C20E-NOTE keep it short", "C20E-PREVIEW one"],
            )],
            lacks: &["Grid"],
            required: true,
        },
        "c20f" => Told {
            step: "m3",
            is_error: None,
            answers: &[],
            lacks: &["\"Yes\"", "\"No\""],
            required: false,
        },
        "c20w" => Told {
            step: "m4",
            is_error: None,
            answers: &[("Should the C20 change go ahead?", &["Yes"])],
            lacks: &["\"No\""],
            required: true,
        },
        _ => return None,
    })
}

/// The text and error flag of the `tool_result` a request body carries for `call`.
pub fn result_in(body: &Value, call: &str) -> Option<(String, bool)> {
    body["messages"]
        .as_array()?
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == call)
        .map(|b| {
            let text = match &b["content"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            (text, b["is_error"].as_bool().unwrap_or(false))
        })
}

/// Checks the question's result in `bodies`, the requests the stand-in matched to `told.step`, and returns it.
pub fn check_told(told: &Told, bodies: &[Value]) -> Result<Option<(String, bool)>, String> {
    let found = bodies.iter().find_map(|b| result_in(b, QUESTION_CALL));
    let Some((text, is_error)) = found else {
        return if told.required {
            Err(format!(
                "no request of step {} carries the question's result",
                told.step
            ))
        } else {
            Ok(None)
        };
    };
    if let Some(want) = told.is_error {
        if is_error != want {
            return Err(format!(
                "the question's result is{} an error result, and the answer given makes it{}: {text}",
                if is_error { "" } else { " not" },
                if want { " one" } else { " none" }
            ));
        }
    }
    for (question, holds) in told.answers {
        let answer = answer_to(&text, question, told.answers)
            .ok_or_else(|| format!("the model is not told the question `{question}`: {text}"))?;
        if let Some(missing) = holds.iter().find(|w| !answer.contains(**w)) {
            return Err(format!(
                "the model is not told `{missing}` as the answer to `{question}` (its answer reads `{answer}`): {text}"
            ));
        }
    }
    if let Some(extra) = told.lacks.iter().find(|w| text.contains(**w)) {
        return Err(format!("the model is told `{extra}`, which nobody answered: {text}"));
    }
    Ok(Some((text, is_error)))
}

/// The part of `text` that answers `question`: from the end of the question's text to the start of the next of
/// `questions` that follows it, or to the end. The CLI writes each question's text before its answer, whether
/// the person answered (`"<question>"="<answer>"`) or went back to the chat (`- "<question>"` and an `Answer:`
/// line under it).
fn answer_to<'t>(text: &'t str, question: &str, questions: &[(&str, &[&str])]) -> Option<&'t str> {
    let start = text.find(question)? + question.len();
    let end = questions
        .iter()
        .filter_map(|(q, _)| text[start..].find(q))
        .min()
        .map_or(text.len(), |at| start + at);
    Some(&text[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(content: Value, is_error: bool) -> Value {
        json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": QUESTION_CALL, "name": "AskUserQuestion", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": QUESTION_CALL, "content": content, "is_error": is_error}]}
        ]})
    }

    const ANSWERED: &str = "User has answered your questions: \"Which colour should the C20 banner use?\"=\"Blue\", \
                            \"Which C20 checks should run?\"=\"Lint, Fuzz\".";

    #[test]
    fn the_answer_given_passes_and_an_option_nobody_picked_fails() {
        let told = told("c20").unwrap();
        let ok = check_told(&told, &[body(json!(ANSWERED), false)]).unwrap();
        assert_eq!(ok.map(|(t, _)| t).as_deref(), Some(ANSWERED));

        let wrong = ANSWERED.replace("\"Blue\"", "\"Green\"");
        let err = check_told(&told, &[body(json!(wrong), false)]).unwrap_err();
        assert!(err.contains("not told `Blue`"), "{err}");

        let extra = ANSWERED.replace("Lint, Fuzz", "Lint, Fuzz, Bench");
        let err = check_told(&told, &[body(json!(extra), false)]).unwrap_err();
        assert!(err.contains("told `Bench`"), "{err}");

        let err = check_told(&told, &[body(json!(ANSWERED), true)]).unwrap_err();
        assert!(err.contains("is an error result"), "{err}");
    }

    /// An option nobody picked fails each row that has one, even when the answer given is there too.
    #[test]
    fn an_unpicked_option_beside_the_answer_fails_every_row() {
        let cases: [(&str, &str, &str, &str); 3] = [
            (
                "c20d",
                r#""Which C20D cache notes apply?"="say \"hi\"", "Redis, not Memcached""#,
                r#""Redis, not Memcached""#,
                "\"Mem\u{202e}cached\", \"Plain\", \"Redis, not Memcached\"",
            ),
            (
                "c20e",
                "\"Which C20E layout reads better?\"=\"List\" selected preview:\n# List\n\n- C20E-PREVIEW one\n- two\n\
                 user notes: C20E-NOTE keep it short",
                "\"List\"",
                "\"List\", \"Grid\"",
            ),
            (
                "c20w",
                "\"Should the C20 change go ahead?\"=\"Yes\"",
                "\"Yes\"",
                "\"Yes\", \"No\"",
            ),
        ];
        for (id, answered, given, widened) in cases {
            let told = told(id).unwrap();
            let text = format!("User has answered your questions: {answered}.");
            assert!(check_told(&told, &[body(json!(text), false)]).is_ok(), "{id}: {text}");
            let extra = text.replacen(given, widened, 1);
            let err = check_told(&told, &[body(json!(extra), false)])
                .expect_err(&format!("{id}: an option nobody picked passes: {extra}"));
            assert!(err.contains("which nobody answered"), "{id}: {err}");
        }
    }

    #[test]
    fn answers_given_under_the_wrong_questions_fail() {
        let pair = told("c20").unwrap();
        let swapped = "User has answered your questions: \"Which colour should the C20 banner use?\"=\"Lint, Fuzz\", \
                       \"Which C20 checks should run?\"=\"Blue\".";
        let err = check_told(&pair, &[body(json!(swapped), false)]).unwrap_err();
        assert!(err.contains("`Blue` as the answer to `Which colour"), "{err}");

        let reordered = "User has answered your questions: \"Which C20 checks should run?\"=\"Lint, Fuzz\", \
                         \"Which colour should the C20 banner use?\"=\"Blue\".";
        assert!(
            check_told(&pair, &[body(json!(reordered), false)]).is_ok(),
            "the CLI's order is not the answer"
        );

        let chat = told("c20c").unwrap();
        let ok =
            "- \"Which colour should the C20 banner use?\"\n  Answer: Blue\n- \"Which C20 checks should run?\"\n  \
                  (No answer provided)";
        assert!(check_told(&chat, &[body(json!(ok), true)]).is_ok());
        let wrong =
            "- \"Which colour should the C20 banner use?\"\n  (No answer provided)\n- \"Which C20 checks should \
                     run?\"\n  Answer: Blue";
        let err = check_told(&chat, &[body(json!(wrong), true)]).unwrap_err();
        assert!(err.contains("`Answer: Blue` as the answer to `Which colour"), "{err}");

        let unasked = ANSWERED.replace("Which C20 checks should run?", "Which checks?");
        let err = check_told(&pair, &[body(json!(unasked), false)]).unwrap_err();
        assert!(
            err.contains("not told the question `Which C20 checks should run?`"),
            "{err}"
        );
    }

    /// Every question an expectation names is one the row's model step asks, word for word, so a renamed
    /// question in a scenario file cannot leave its answer unchecked.
    #[test]
    fn every_expected_question_is_one_the_row_asks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/claude_scenarios");
        for id in [
            "c20",
            "c20_auto",
            "c20_manual",
            "c20x",
            "c20c",
            "c20d",
            "c20e",
            "c20f",
            "c20w",
        ] {
            let told = told(id).unwrap();
            let file: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{id}.json"))).unwrap())
                .unwrap_or_else(|e| panic!("{id}: {e}"));
            let asked: Vec<&str> = file["model"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|step| step["reply"].as_array().into_iter().flatten())
                .filter(|part| part["tool_use"]["id"] == QUESTION_CALL)
                .flat_map(|part| part["tool_use"]["input"]["questions"].as_array().into_iter().flatten())
                .filter_map(|q| q["question"].as_str())
                .collect();
            assert!(!asked.is_empty(), "{id} scripts no question");
            for (question, _) in told.answers {
                assert!(asked.contains(question), "{id} does not ask `{question}`: {asked:?}");
            }
        }
    }

    #[test]
    fn a_result_in_parts_is_read_whole_and_a_missing_one_fails_only_when_required() {
        let parts = json!([
            {"type": "text", "text": "- \"Which colour should the C20 banner use?\"\n  Answer: Blue"},
            {"type": "text", "text": "- \"Which C20 checks should run?\"\n  (No answer provided)"}
        ]);
        assert!(check_told(&told("c20c").unwrap(), &[body(parts, true)]).is_ok());

        let none = json!({"messages": [{"role": "user", "content": "go on"}]});
        assert!(check_told(&told("c20c").unwrap(), std::slice::from_ref(&none)).is_err());
        assert_eq!(check_told(&told("c20f").unwrap(), &[none]), Ok(None));
    }

    #[test]
    fn only_the_rows_whose_answer_reaches_the_model_have_an_expectation() {
        for id in [
            "c20",
            "c20_auto",
            "c20_manual",
            "c20x",
            "c20c",
            "c20d",
            "c20e",
            "c20f",
            "c20w",
        ] {
            assert!(told(id).is_some(), "{id}");
        }
        for id in ["c20b", "c20g", "c20s", "c20_off"] {
            assert!(told(id).is_none(), "{id}");
        }
    }
}

//! Bounded UI sequences. A failed or uncertain step stops the sequence without replay.

use nuphus_browser::{BrowserClient, BrowserError};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;

pub(super) enum Plan<'a> {
    Steps(&'a [Value]),
    Script(&'a str),
}

fn error(message: impl Into<String>) -> BrowserError {
    BrowserError::Execution(message.into())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Empty script/steps values are strict-client fillers, not a second plan.
pub(super) fn plan(args: &Value) -> Result<Plan<'_>, BrowserError> {
    let script = match args.get("script") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::String(value)) => Some(value.as_str()),
        _ => return Err(error("browser_exec script must be a string")),
    };
    let steps = match args.get("steps") {
        None | Some(Value::Null) => None,
        Some(Value::Array(steps)) if steps.is_empty() => None,
        Some(Value::Array(steps)) => Some(steps.as_slice()),
        _ => return Err(error("browser_exec steps must be an array")),
    };
    match (script, steps) {
        (None, Some(steps)) => {
            validate_steps(steps)?;
            Ok(Plan::Steps(steps))
        }
        (Some(script), None) => Ok(Plan::Script(script)),
        (Some(_), Some(_)) => Err(error("Provide script or steps, not both")),
        (None, None) => Err(error("browser_exec requires script or steps")),
    }
}

pub(super) fn validate_steps(steps: &[Value]) -> Result<(), BrowserError> {
    if steps.is_empty() || steps.len() > 12 {
        return Err(error("browser_exec steps must contain 1..12 actions"));
    }
    for (index, step) in steps.iter().enumerate() {
        let object = step
            .as_object()
            .ok_or_else(|| error("Each UI step must be an object"))?;
        let op = text(step, "op");
        let allowed: &[&str] = match op {
            "type" => &["op", "ref", "text", "mode", "clear"],
            "click" => &["op", "ref", "trusted"],
            "press" => &["op", "key"],
            "read" => &[
                "op",
                "ref",
                "selector",
                "until_text",
                "completion_prefix",
                "timeout_ms",
                "max_chars",
            ],
            "evaluate" => &["op", "ref", "script"],
            "snapshot" => &["op"],
            _ => return Err(error(format!("Unsupported UI step: {op}"))),
        };
        for (field, value) in object {
            if !allowed.contains(&field.as_str()) {
                return Err(error(format!("{op} step does not accept {field}")));
            }
            let valid = match field.as_str() {
                "clear" | "trusted" => value.is_boolean(),
                "timeout_ms" => value.as_u64().is_some_and(|n| n <= 8000),
                "max_chars" => value.as_u64().is_some_and(|n| (1..=20000).contains(&n)),
                "text" | "selector" | "until_text" | "completion_prefix" => value.is_string(),
                "ref" if op == "evaluate" => value.is_string(),
                _ => value.as_str().is_some_and(|s| !s.trim().is_empty()),
            };
            if !valid {
                return Err(error(format!("Invalid {field} in {op} step")));
            }
        }
        let reference = text(step, "ref");
        if !reference.is_empty()
            && !reference.strip_prefix('@').is_some_and(|digits| {
                !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && digits.parse::<usize>().is_ok_and(|n| n > 0)
            })
        {
            return Err(error("UI step ref must be a current snapshot @N"));
        }
        if op == "snapshot" && index + 1 != steps.len() {
            return Err(error(
                "snapshot must be the final UI step because it renumbers refs",
            ));
        }
        let required: &[&str] = match op {
            "type" | "click" | "read" => &["ref"],
            "press" => &["key"],
            "evaluate" => &["script"],
            _ => &[],
        };
        for field in required {
            if text(step, field).trim().is_empty() {
                return Err(error(format!("{op} step requires {field}")));
            }
        }
        if op == "press" {
            BrowserClient::validate_key_chord(text(step, "key"))?;
        }
        if op == "type" {
            if step.get("text").and_then(Value::as_str).is_none() {
                return Err(error("type step requires text"));
            }
            if !matches!(
                step.get("mode").and_then(Value::as_str).unwrap_or("insert"),
                "insert" | "paste"
            ) {
                return Err(error("type mode must be insert or paste"));
            }
            if text(step, "mode") == "paste"
                && step.get("clear").and_then(Value::as_bool) == Some(true)
            {
                return Err(error(
                    "paste steps require clear:false; paste does not clear a terminal buffer",
                ));
            }
        }
        let prefix = text(step, "completion_prefix");
        if !prefix.is_empty() {
            if !text(step, "until_text").is_empty() {
                return Err(error(
                    "read accepts until_text or completion_prefix, not both",
                ));
            }
            if prefix.len() > 128
                || !prefix.ends_with('=')
                || !prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_:-.=".contains(&b))
            {
                return Err(error(
                    "completion_prefix must be 1..128 ASCII letters/digits/_:-.=, ending in =",
                ));
            }
        }
    }
    Ok(())
}

trait Driver {
    async fn perform(&mut self, step: &Value) -> Result<Value, BrowserError>;
}

struct BrowserDriver<'a>(&'a mut BrowserClient);

impl Driver for BrowserDriver<'_> {
    async fn perform(&mut self, step: &Value) -> Result<Value, BrowserError> {
        let client = &mut *self.0;
        let value = match text(step, "op") {
            "type" => Value::String(if text(step, "mode") == "paste" {
                client
                    .paste_text_at_ref(text(step, "ref"), text(step, "text"))
                    .await?
            } else {
                client
                    .type_text_with_options(
                        text(step, "ref"),
                        text(step, "text"),
                        step.get("clear").and_then(Value::as_bool).unwrap_or(false),
                    )
                    .await?
            }),
            "press" => Value::String(client.press_key(text(step, "key")).await?),
            "click" => Value::String(
                if step
                    .get("trusted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    client.click_trusted(text(step, "ref"), "left").await?
                } else {
                    client.click(text(step, "ref")).await?
                },
            ),
            "evaluate" => {
                if text(step, "ref").is_empty() {
                    client.evaluate(text(step, "script")).await?
                } else {
                    client
                        .evaluate_at_ref(text(step, "script"), text(step, "ref"))
                        .await?
                }
            }
            "snapshot" => Value::String(client.snapshot(false, None).await?),
            "read" => return read(client, step).await,
            _ => unreachable!("validated before dispatch"),
        };
        Ok(json!({"result":value}))
    }
}

async fn read(client: &mut BrowserClient, step: &Value) -> Result<Value, BrowserError> {
    let selector = match text(step, "selector") {
        "" => ".xterm-accessibility-tree",
        selector => selector,
    };
    let options = json!({
        "selector":selector,
        "max_chars":step.get("max_chars").and_then(Value::as_u64).unwrap_or(6000),
        "until_text":text(step,"until_text"),
        "completion_prefix":text(step,"completion_prefix")
    });
    // Use the documented async-expression contract explicitly, including when
    // user-provided text happens to contain the word used by the evaluate wrapper.
    let script = format!(
        "return await ({}).call(this, {});",
        include_str!("dom_read.js"),
        options
    );
    let deadline = Instant::now()
        + Duration::from_millis(step.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0));
    loop {
        let mut value = client.evaluate_at_ref(&script, text(step, "ref")).await?;
        if !value.is_object() {
            return Err(error("DOM read returned an invalid receipt"));
        }
        if value["ambiguous"] == true {
            value["success"] = json!(false);
            value["status"] = json!("unknown");
            value["error"] = json!("Completion marker occurs more than once; inspect the existing run and use unique markers");
            return Ok(value);
        }
        if value["matched"] == true {
            if value
                .get("exit_code")
                .and_then(Value::as_u64)
                .is_some_and(|code| code != 0)
            {
                value["success"] = json!(false);
                value["status"] = json!("failed");
                value["error"] = json!(format!(
                    "Terminal command completed with exit code {}",
                    value["exit_code"]
                ));
            }
            return Ok(value);
        }
        if Instant::now() >= deadline {
            value["success"] = json!(false);
            value["status"] = json!("unknown");
            value["error"] = json!("Expected DOM completion not observed within the read budget; inspect the existing run without resubmitting it");
            return Ok(value);
        }
        tokio::time::sleep_until((Instant::now() + Duration::from_millis(75)).min(deadline)).await;
    }
}

/// Each await is bounded here, where the completed-step journal is still owned.
/// An outer timeout would discard that journal and hide earlier completed writes.
async fn run<D: Driver>(
    driver: &mut D,
    steps: &[Value],
    budget: Duration,
) -> Result<String, BrowserError> {
    validate_steps(steps)?;
    let start = Instant::now();
    let deadline = start + budget;
    let mut results = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let started = Instant::now();
        let mut row = if started >= deadline {
            json!({"success":false,"status":"not_started","attempted":false,"error":"Sequence budget exhausted before this step; it was not dispatched"})
        } else {
            let mut row = match tokio::time::timeout_at(deadline, driver.perform(step)).await {
                Ok(Ok(row)) => row,
                Ok(Err(error)) => {
                    json!({"success":false,"status":"unknown","error":error.to_string()})
                }
                Err(_) => {
                    json!({"success":false,"status":"unknown","error":"Sequence budget exhausted during this step; it may have taken effect. Inspect state before any retry"})
                }
            };
            row["attempted"] = json!(true);
            row
        };
        if row.get("success").is_none() {
            row["success"] = json!(true);
        }
        if row.get("status").is_none() {
            row["status"] = json!("completed");
        }
        row["op"] = step["op"].clone();
        row["index"] = json!(index);
        row["elapsed_ms"] = json!(started.elapsed().as_millis());
        let success = row["success"] == true;
        results.push(row);
        if !success {
            break;
        }
    }
    let success = results.len() == steps.len() && results.iter().all(|r| r["success"] == true);
    let status = if success {
        "completed"
    } else {
        results
            .last()
            .and_then(|r| r["status"].as_str())
            .unwrap_or("failed")
    };
    Ok(json!({
        "success":success,"status":status,"stopped":!success,"replayed":false,
        "completed_steps":results.iter().filter(|r| r["success"] == true).count(),
        "attempted_steps":results.iter().filter(|r| r["attempted"] == true).count(),
        "skipped_steps":steps.len()-results.iter().filter(|r| r["attempted"] == true).count(),
        "steps":results,"elapsed_ms":start.elapsed().as_millis()
    })
    .to_string())
}

pub(super) async fn execute(
    client: &mut BrowserClient,
    steps: &[Value],
) -> Result<String, BrowserError> {
    run(&mut BrowserDriver(client), steps, Duration::from_secs(15)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_empty_script_filler_but_rejects_two_plans() {
        let args = json!({"script":"", "steps":[{"op":"evaluate","script":"1 + 1"}]});
        assert!(matches!(plan(&args).unwrap(), Plan::Steps(_)));
        assert!(matches!(
            plan(&json!({"script":"1", "steps":[]})).unwrap(),
            Plan::Script(_)
        ));
        assert!(plan(&json!({"script":"1", "steps":[{"op":"snapshot"}]})).is_err());
        for args in [
            json!({}),
            json!({"script":""}),
            json!({"script":1}),
            json!({"steps":{}}),
            json!({"steps":[]}),
        ] {
            assert!(plan(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn validates_receipt_contract_and_optional_empty_read_fields() {
        assert!(validate_steps(&[json!({"op":"read","ref":"@1","selector":"","until_text":"","completion_prefix":"TASK_run_RC=","timeout_ms":0})]).is_ok());
        assert!(validate_steps(&[json!({"op":"evaluate","script":"1","ref":""})]).is_ok());
        for prefix in ["bad prefix=", "missing_equals", "line\n=", "é="] {
            assert!(
                validate_steps(&[json!({"op":"read","ref":"@1","completion_prefix":prefix})])
                    .is_err()
            );
        }
        assert!(validate_steps(&[
            json!({"op":"read","ref":"@1","until_text":"done","completion_prefix":"TASK_RC="})
        ])
        .is_err());
        assert!(validate_steps(&[
            json!({"op":"type","ref":"@1","text":"x","mode":"paste","clear":true})
        ])
        .is_err());
    }

    struct FakeDriver {
        calls: usize,
        pending_at: Option<usize>,
        fail_at: Option<usize>,
    }
    impl Driver for FakeDriver {
        async fn perform(&mut self, _: &Value) -> Result<Value, BrowserError> {
            let index = self.calls;
            self.calls += 1; // Models a dispatched side effect, before its receipt arrives.
            if self.pending_at == Some(index) {
                std::future::pending::<()>().await;
            }
            if self.fail_at == Some(index) {
                return Ok(
                    json!({"result":"receipt\n","success":false,"status":"failed","exit_code":13}),
                );
            }
            Ok(json!({"result":index}))
        }
    }

    fn actions() -> Vec<Value> {
        (0..3)
            .map(|_| json!({"op":"evaluate","script":"1"}))
            .collect()
    }

    #[tokio::test]
    async fn whole_plan_is_validated_before_any_driver_call() {
        let mut driver = FakeDriver {
            calls: 0,
            pending_at: None,
            fail_at: None,
        };
        assert!(run(
            &mut driver,
            &[actions()[0].clone(), json!({"op":"typo"})],
            Duration::from_secs(1)
        )
        .await
        .is_err());
        for key in ["Control+", "Hyper+x", "NotAKey"] {
            assert!(run(
                &mut driver,
                &[actions()[0].clone(), json!({"op":"press","key":key})],
                Duration::from_secs(1)
            )
            .await
            .is_err());
        }
        assert_eq!(driver.calls, 0);
    }

    #[tokio::test]
    async fn timeout_retains_prior_steps_and_never_replays_a_pending_write() {
        let mut driver = FakeDriver {
            calls: 0,
            pending_at: Some(1),
            fail_at: None,
        };
        let output: Value = serde_json::from_str(
            &run(&mut driver, &actions(), Duration::from_millis(20))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(driver.calls, 2);
        assert_eq!(output["steps"][0]["result"], 0);
        assert_eq!(output["steps"][1]["status"], "unknown");
        assert_eq!(output["completed_steps"], 1);
        assert_eq!(output["skipped_steps"], 1);
        assert_eq!(output["success"], false);
        assert_eq!(output["replayed"], false);
    }

    #[tokio::test]
    async fn nonzero_receipt_stops_following_actions() {
        let mut driver = FakeDriver {
            calls: 0,
            pending_at: None,
            fail_at: Some(1),
        };
        let output: Value = serde_json::from_str(
            &run(&mut driver, &actions(), Duration::from_secs(1))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(driver.calls, 2);
        assert_eq!(output["steps"][1]["exit_code"], 13);
        assert_eq!(output["status"], "failed");
        assert_eq!(output["success"], false);
    }

    #[tokio::test]
    async fn expired_budget_does_not_dispatch_the_first_action() {
        let mut driver = FakeDriver {
            calls: 0,
            pending_at: None,
            fail_at: None,
        };
        let output: Value =
            serde_json::from_str(&run(&mut driver, &actions(), Duration::ZERO).await.unwrap())
                .unwrap();
        assert_eq!(driver.calls, 0);
        assert_eq!(output["steps"][0]["status"], "not_started");
        assert_eq!(output["skipped_steps"], 3);
    }
}

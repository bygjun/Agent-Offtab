use crate::{
    mobile_reply::ResultCode,
    model::{ChoiceRequest, Session},
};
use serde_json::{json, Map, Value};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::net::UnixStream,
    path::Path,
    thread,
    time::{Duration, Instant},
};

const MAX_FRAME: u64 = 512 * 1024;

pub fn answer(
    socket: &Path,
    session: &Session,
    choice: &ChoiceRequest,
    selected: &[usize],
) -> ResultCode {
    let Some(path) = session.transcript_path.as_deref().and_then(Path::to_str) else {
        return ResultCode::Unavailable;
    };
    let Some(answers) = chosen_answers(choice, selected) else {
        return ResultCode::Blocked;
    };
    let Ok(mut transcript) = File::open(path) else {
        return ResultCode::Unavailable;
    };
    let Ok(before) = transcript.metadata() else {
        return ResultCode::Unavailable;
    };
    let Ok(stream) = UnixStream::connect(socket) else {
        return ResultCode::Unavailable;
    };
    let Ok(read) = stream.try_clone() else {
        return ResultCode::Unavailable;
    };
    if read.set_read_timeout(Some(Duration::from_secs(2))).is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .is_err()
    {
        return ResultCode::Unavailable;
    }
    let outcome = submit(
        stream,
        BufReader::new(read),
        &session.id,
        path,
        choice,
        &answers,
    );
    if outcome != ResultCode::Sent {
        return outcome;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match recorded_answer(&mut transcript, before.len(), &choice.call_id, &answers) {
            Ok(true) => return ResultCode::Sent,
            Ok(false) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            _ => return ResultCode::Unknown,
        }
    }
}

fn recorded_answer(
    file: &mut File,
    start: u64,
    call_id: &str,
    answers: &Value,
) -> io::Result<bool> {
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(4 * MAX_FRAME));
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? > 0 && line.ends_with(b"\n") {
        if let Ok(event) = serde_json::from_slice::<Value>(&line) {
            let payload = &event["payload"];
            if event["type"] == "response_item"
                && payload["type"] == "function_call_output"
                && payload["call_id"] == call_id
            {
                let content = &payload["output"];
                let body = content.as_str().or_else(|| {
                    let [part] = content.as_array()?.as_slice() else {
                        return None;
                    };
                    part["text"].as_str()
                });
                return Ok(body
                    .and_then(|value| serde_json::from_str::<Value>(value).ok())
                    .is_some_and(|value| &value["answers"] == answers));
            }
        }
        line.clear();
    }
    Ok(false)
}

fn submit(
    mut writer: impl Write,
    mut reader: impl BufRead,
    thread: &str,
    path: &str,
    choice: &ChoiceRequest,
    answers: &Value,
) -> ResultCode {
    let deadline = Instant::now() + Duration::from_secs(8);
    let send = |writer: &mut dyn Write, message: Value| -> io::Result<()> {
        serde_json::to_writer(&mut *writer, &message)?;
        writer.write_all(b"\n")?;
        writer.flush()
    };
    let next = |reader: &mut dyn BufRead| -> Option<Value> {
        let mut line = Vec::new();
        if Instant::now() >= deadline
            || reader.take(MAX_FRAME).read_until(b'\n', &mut line).ok()? == 0
            || !line.ends_with(b"\n")
        {
            return None;
        }
        serde_json::from_slice(&line).ok()
    };
    let response = |reader: &mut dyn BufRead, id| -> Option<Value> {
        for _ in 0..128 {
            let message = next(reader)?;
            if message["id"] == id && message.get("method").is_none() {
                return message.get("result").cloned();
            }
        }
        None
    };
    if send(&mut writer, json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"offtab","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}})).is_err()
        || response(&mut reader, 1).is_none()
        || send(&mut writer, json!({"method":"initialized","params":{}})).is_err()
        || send(&mut writer, json!({"id":2,"method":"thread/loaded/list"})).is_err()
        || !response(&mut reader, 2).and_then(|value| value["data"].as_array().map(|rows| rows.iter().any(|row| row == thread))).unwrap_or(false)
        || send(&mut writer, json!({"id":3,"method":"thread/resume","params":{"threadId":thread,"path":path,"excludeTurns":true}})).is_err() {
        return ResultCode::Unavailable;
    }
    let mut resumed = false;
    let mut pending = None;
    for _ in 0..128 {
        let Some(message) = next(&mut reader) else {
            break;
        };
        match message["method"].as_str() {
            Some("item/tool/requestUserInput")
                if matches_request(&message["params"], thread, choice) =>
            {
                if pending.is_some() || !wire_id(&message["id"]) {
                    return ResultCode::Busy;
                }
                pending = Some(message["id"].clone());
            }
            Some("serverRequest/resolved")
                if pending.as_ref() == Some(&message["params"]["requestId"]) =>
            {
                return ResultCode::Busy
            }
            None if message["id"] == 3 => {
                if message["result"]["thread"]["id"] != thread {
                    return ResultCode::Unavailable;
                }
                resumed = true;
            }
            _ => {}
        }
        if resumed && pending.is_some() {
            break;
        }
    }
    let Some(id) = pending.filter(|_| resumed) else {
        return ResultCode::Busy;
    };
    if send(&mut writer, json!({"id":id,"result":{"answers":answers}})).is_err() {
        return ResultCode::Unknown;
    }
    for _ in 0..128 {
        let Some(message) = next(&mut reader) else {
            break;
        };
        if message["method"] == "serverRequest/resolved"
            && message["params"]["threadId"] == thread
            && message["params"]["requestId"] == id
        {
            return ResultCode::Sent;
        }
    }
    ResultCode::Unknown
}

fn chosen_answers(choice: &ChoiceRequest, selected: &[usize]) -> Option<Value> {
    if selected.len() != choice.questions.len() || selected.is_empty() {
        return None;
    }
    let answers = choice
        .questions
        .iter()
        .zip(selected)
        .map(|(question, &index)| {
            question
                .options
                .get(index)
                .map(|answer| (question.id.clone(), json!({"answers":[answer]})))
        })
        .collect::<Option<Map<_, _>>>()?;
    (answers.len() == selected.len()).then_some(Value::Object(answers))
}

fn wire_id(id: &Value) -> bool {
    id.is_u64() || id.as_str().is_some_and(|value| !value.is_empty())
}

fn matches_request(params: &Value, thread: &str, choice: &ChoiceRequest) -> bool {
    let item = &params["itemId"];
    params["threadId"] == thread
        && (item == &choice.call_id || (!choice.item_id.is_empty() && item == &choice.item_id))
        && params["questions"].as_array().is_some_and(|rows| {
            rows.len() == choice.questions.len()
                && rows.iter().zip(&choice.questions).all(|(row, question)| {
                    row["id"] == question.id
                        && row["question"] == question.question
                        && row["isSecret"] != true
                        && row["options"].as_array().is_some_and(|options| {
                            options.len() == question.options.len()
                                && options
                                    .iter()
                                    .zip(&question.options)
                                    .all(|(option, label)| option["label"] == *label)
                        })
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChoiceQuestion;
    use std::io::Cursor;

    fn fixture() -> ChoiceRequest {
        ChoiceRequest {
            call_id: "call1".into(),
            item_id: String::new(),
            questions: vec![ChoiceQuestion {
                id: "colour".into(),
                question: "Which colour?".into(),
                options: vec!["Blue".into(), "Red".into()],
            }],
        }
    }

    fn exchange(
        item: &str,
        thread: &str,
        question: &str,
        method: &str,
    ) -> (ResultCode, Vec<Value>) {
        let frames = [
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"data":["thread"]}}),
            json!({"id":"provider-request","method":method,"params":{"itemId":item,"threadId":thread,"questions":[{"id":"colour","question":question,"options":[{"label":"Blue"},{"label":"Red"}]}]}}),
            json!({"id":3,"result":{"thread":{"id":"thread"}}}),
            json!({"method":"serverRequest/resolved","params":{"threadId":"thread","requestId":"provider-request"}}),
        ].iter().map(|value| format!("{value}\n")).collect::<String>();
        let choice = fixture();
        let answers = chosen_answers(&choice, &[1]).unwrap();
        let mut output = Vec::new();
        let result = submit(
            &mut output,
            Cursor::new(frames),
            "thread",
            "/test/thread.jsonl",
            &choice,
            &answers,
        );
        (
            result,
            output
                .split(|&byte| byte == b'\n')
                .filter_map(|line| serde_json::from_slice(line).ok())
                .collect(),
        )
    }

    #[test]
    fn only_the_exact_live_question_receives_an_answer() {
        let correct = (
            "call1",
            "thread",
            "Which colour?",
            "item/tool/requestUserInput",
        );
        let (result, written) = exchange(correct.0, correct.1, correct.2, correct.3);
        assert_eq!(result, ResultCode::Sent);
        assert_eq!(
            written.last().unwrap(),
            &json!({"id":"provider-request","result":{"answers":{"colour":{"answers":["Red"]}}}})
        );
        for fields in [
            ("old-call", correct.1, correct.2, correct.3),
            (correct.0, "other-thread", correct.2, correct.3),
            (correct.0, correct.1, "New question?", correct.3),
            (
                correct.0,
                correct.1,
                correct.2,
                "item/commandExecution/requestApproval",
            ),
        ] {
            let (result, written) = exchange(fields.0, fields.1, fields.2, fields.3);
            assert_eq!(result, ResultCode::Busy);
            assert!(written.iter().all(|frame| frame.get("result").is_none()));
        }
    }

    #[test]
    fn provider_receipt_must_contain_the_selected_answer() {
        let mut transcript = tempfile::tempfile().unwrap();
        let choice = fixture();
        let expected = chosen_answers(&choice, &[1]).unwrap();
        let reported = chosen_answers(&choice, &[0]).unwrap();
        let event = json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call1","output":json!({"answers":reported}).to_string()}});
        writeln!(transcript, "{event}").unwrap();
        assert!(!recorded_answer(&mut transcript, 0, &choice.call_id, &expected).unwrap());
        assert!(recorded_answer(&mut transcript, 0, &choice.call_id, &reported).unwrap());
    }
}

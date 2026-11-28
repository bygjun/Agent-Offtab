use crate::{
    claude_pasteboard::FilePaste,
    mobile_attachment::Attachment,
    mobile_reply::ResultCode,
    model::{ChoiceRequest, OpenResult, Provider, Session},
    targets::{self, CLAUDE_BUNDLE},
    transcript,
};
use core_foundation::{
    array::CFArray,
    base::{CFType, CFTypeID, CFTypeRef, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
    url::CFURL,
};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWorkspace;
use std::{
    collections::VecDeque,
    fs::{self, File},
    path::{Path, PathBuf},
    ptr,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

const PROMPT: &[&str] = &["Prompt", "프롬프트"];
const NEXT_STEPS: &[&str] = &["Recommended next steps", "추천 다음 작업"];
const SEND: &[&str] = &[
    "Send",
    "Send message",
    "Send prompt",
    "보내기",
    "전송",
    "메시지 보내기",
    "메시지 전송",
    "프롬프트 보내기",
];
const STOP: &[&str] = &[
    "Stop",
    "Stop response",
    "Stop generating",
    "중지",
    "응답 중지",
    "생성 중지",
];
const V_KEY: u16 = 9;
const COMMAND_FLAG: u64 = 1 << 20;
const POLL: Duration = Duration::from_millis(90);
const COMPOSER_NODES: usize = 2_000;
static DELIVERY: Mutex<()> = Mutex::new(());

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> CFTypeRef;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: CFTypeRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGEventCreateKeyboardEvent(source: CFTypeRef, key: u16, down: bool) -> CFTypeRef;
    fn CGEventSetFlags(event: CFTypeRef, flags: u64);
    fn CGEventPostToPid(pid: libc::pid_t, event: CFTypeRef);
}

pub fn trusted() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(ptr::null()) != 0 }
}

pub fn request_access() -> bool {
    let prompt = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let options = CFDictionary::from_CFType_pairs(&[(prompt, CFBoolean::true_value())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
}

fn unlocked() -> bool {
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return false;
    }
    let state = unsafe { CFDictionary::<CFString, CFType>::wrap_under_create_rule(raw) };
    let flag = |key: &str| {
        state
            .find(&CFString::new(key))?
            .downcast::<CFBoolean>()
            .map(bool::from)
    };
    let owner = state
        .find(&CFString::new("kCGSSessionUserIDKey"))
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|value| value.to_i64());
    owner == Some(i64::from(unsafe { libc::getuid() }))
        && flag("kCGSSessionOnConsoleKey") == Some(true)
        && flag("kCGSessionLoginDoneKey") == Some(true)
        && flag("CGSSessionScreenIsLocked") != Some(true)
}

#[derive(Clone, PartialEq)]
struct Element(CFType);

impl Element {
    fn from_value(value: CFType) -> Option<Self> {
        (value.type_of() == unsafe { AXUIElementGetTypeID() }).then_some(Self(value))
    }

    fn attribute(&self, name: &str) -> Option<CFType> {
        let mut value = ptr::null();
        let code = unsafe {
            AXUIElementCopyAttributeValue(
                self.0.as_CFTypeRef(),
                CFString::new(name).as_concrete_TypeRef(),
                &mut value,
            )
        };
        (code == 0 && !value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
    }

    fn related(&self, name: &str) -> Option<Self> {
        Self::from_value(self.attribute(name)?)
    }

    fn string(&self, name: &str) -> Option<String> {
        self.attribute(name)?
            .downcast_into::<CFString>()
            .map(|value| value.to_string())
    }

    fn role(&self, name: &str) -> bool {
        self.string("AXRole").as_deref() == Some(name)
    }

    fn named(&self, names: &[&str]) -> bool {
        ["AXTitle", "AXDescription"].iter().any(|key| {
            self.string(key)
                .is_some_and(|name| names.iter().any(|known| name.eq_ignore_ascii_case(known)))
        })
    }

    fn enabled(&self) -> bool {
        self.attribute("AXEnabled")
            .and_then(|value| value.downcast_into::<CFBoolean>())
            .is_some_and(bool::from)
    }

    fn url(&self) -> Option<String> {
        let raw = self.attribute("AXURL")?;
        raw.downcast::<CFURL>()
            .map(|url| url.get_string().to_string())
            .or_else(|| {
                raw.downcast_into::<CFString>()
                    .map(|value| value.to_string())
            })
    }

    fn children(&self) -> impl Iterator<Item = Self> {
        let values = self
            .attribute("AXChildren")
            .and_then(|value| value.downcast_into::<CFArray>());
        values.into_iter().flat_map(|values| {
            values
                .get_all_values()
                .into_iter()
                .filter_map(|raw| {
                    (!raw.is_null())
                        .then(|| unsafe { CFType::wrap_under_get_rule(raw) })
                        .and_then(Self::from_value)
                })
                .collect::<Vec<_>>()
        })
    }

    fn set_text(&self, text: &str) -> bool {
        let attribute = CFString::new("AXValue");
        let mut settable = 0;
        unsafe {
            AXUIElementIsAttributeSettable(
                self.0.as_CFTypeRef(),
                attribute.as_concrete_TypeRef(),
                &mut settable,
            ) == 0
                && settable != 0
                && AXUIElementSetAttributeValue(
                    self.0.as_CFTypeRef(),
                    attribute.as_concrete_TypeRef(),
                    CFString::new(text).as_CFTypeRef(),
                ) == 0
        }
    }
}

fn is_session(url: &str, id: &str) -> bool {
    url.strip_prefix("https://claude.ai/epitaxy/") == Some(id)
}

struct Located {
    element: Element,
    ancestors: [Option<Element>; 5],
}

fn find_unique(
    root: &Element,
    limit: usize,
    deadline: Instant,
    matches: impl Fn(&Element) -> bool,
    skip: impl Fn(&Element) -> bool,
) -> Option<Located> {
    let mut nodes: VecDeque<_> = root
        .children()
        .map(|element| Located {
            element,
            ancestors: [Some(root.clone()), None, None, None, None],
        })
        .collect();
    let mut matched = None;
    for _ in 0..limit {
        let Some(node) = nodes.pop_front() else {
            return matched;
        };
        if Instant::now() >= deadline {
            return None;
        }
        if matches(&node.element) {
            if matched.is_some() {
                return None;
            }
            matched = Some(node);
        } else if !skip(&node.element) {
            nodes.extend(node.element.children().map(|element| Located {
                element,
                ancestors: [
                    Some(node.element.clone()),
                    node.ancestors[0].clone(),
                    node.ancestors[1].clone(),
                    node.ancestors[2].clone(),
                    node.ancestors[3].clone(),
                ],
            }));
        }
    }
    if nodes.is_empty() {
        matched
    } else {
        None
    }
}

struct Composer {
    pid: libc::pid_t,
    application: Element,
    window: Element,
    page: Element,
    prompt: Element,
    controls: [Option<Element>; 5],
}

fn frontmost_claude_pid() -> Option<libc::pid_t> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    (app.bundleIdentifier()?.to_string() == CLAUDE_BUNDLE).then(|| app.processIdentifier())
}

fn application(pid: libc::pid_t) -> Option<Element> {
    let raw = unsafe { AXUIElementCreateApplication(pid) };
    if raw.is_null() {
        return None;
    }
    let app = Element(unsafe { CFType::wrap_under_create_rule(raw) });
    unsafe { AXUIElementSetMessagingTimeout(app.0.as_CFTypeRef(), 0.2) };
    Some(app)
}

impl Composer {
    fn find(id: &str, deadline: Instant) -> Option<Self> {
        let pid = frontmost_claude_pid()?;
        Self::for_app(pid, id, deadline)
    }

    fn for_app(pid: libc::pid_t, id: &str, deadline: Instant) -> Option<Self> {
        let application = application(pid)?;
        let window = application.related("AXFocusedWindow")?;
        let page = find_unique(
            &window,
            160,
            deadline,
            |node| node.role("AXWebArea") && node.url().is_some_and(|url| is_session(&url, id)),
            |node| node.role("AXWebArea"),
        )?
        .element;
        let prompt = find_unique(
            &page,
            COMPOSER_NODES,
            deadline,
            |node| node.role("AXTextArea") && node.named(PROMPT),
            |node| node.role("AXWebArea") || node.named(&["Chat messages", "채팅 메시지"]),
        )?;
        // Claude's embedded web view can omit ancestors from AXParent.
        // Keep the composer controls from the verified document's child tree.
        Some(Self {
            pid,
            application,
            window,
            page,
            prompt: prompt.element,
            controls: prompt.ancestors,
        })
    }

    fn current(&self, id: &str) -> bool {
        unlocked()
            && frontmost_claude_pid() == Some(self.pid)
            && self.application.related("AXFocusedWindow").as_ref() == Some(&self.window)
            && self.page.url().is_some_and(|url| is_session(&url, id))
    }

    fn button(&self, names: &[&str], deadline: Instant) -> Option<Element> {
        self.controls[..4].iter().flatten().find_map(|scope| {
            if scope == &self.page {
                return None;
            }
            find_unique(
                scope,
                80,
                deadline,
                |node| node.role("AXButton") && node.named(names),
                |node| {
                    node.role("AXTextArea")
                        || node.role("AXWebArea")
                        || node.named(&["Chat messages", "채팅 메시지"])
                },
            )
            .map(|found| found.element)
        })
    }

    fn contains_draft(&self, text: &str) -> bool {
        self.prompt
            .string("AXValue")
            .is_some_and(|value| value == text || value.strip_suffix('\n') == Some(text))
    }

    fn focus(&self, element: &Element) -> bool {
        let attribute = CFString::new("AXFocused");
        unsafe {
            AXUIElementSetAttributeValue(
                element.0.as_CFTypeRef(),
                attribute.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            ) == 0
        }
    }

    fn focused(&self, element: &Element) -> bool {
        self.application.related("AXFocusedUIElement").as_ref() == Some(element)
    }

    fn focus_prompt(&self) -> bool {
        self.focus(&self.prompt)
    }

    fn focused_prompt(&self) -> bool {
        self.focused(&self.prompt)
    }

    fn activate_send_key(
        &self,
        id: &str,
        button: &Element,
        ready: impl FnOnce() -> bool,
    ) -> Option<()> {
        const SPACE_KEY: u16 = 49;
        if !unsafe { CGPreflightPostEventAccess() } || !self.current(id) || !self.focus(button) {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(350);
        while self.current(id) {
            if self.focused(button) {
                if ready() && self.focused(button) && self.current(id) {
                    return post_key(self.pid, SPACE_KEY, 0);
                }
                return None;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL);
        }
        None
    }

    fn focus_current_prompt(&self, id: &str) -> bool {
        if !self.current(id) || (!self.focused_prompt() && !self.focus_prompt()) {
            return false;
        }
        let deadline = Instant::now() + Duration::from_millis(800);
        while self.current(id) {
            if self.focused_prompt() {
                return true;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL);
        }
        false
    }

    fn attachment_buttons(&self, deadline: Instant) -> Option<Vec<(String, Element)>> {
        // The attachment tray and input controls are siblings in Claude’s AX tree.
        let scope = self.controls[4]
            .as_ref()
            .filter(|scope| *scope != &self.page)?;
        let mut nodes: VecDeque<_> = scope.children().collect();
        let mut found = Vec::new();
        for _ in 0..80 {
            let Some(node) = nodes.pop_front() else {
                return Some(found);
            };
            if Instant::now() >= deadline {
                return None;
            }
            if node.role("AXButton") {
                let name = ["AXTitle", "AXDescription"].iter().find_map(|attr| {
                    let label = node.string(attr)?;
                    let file = label
                        .strip_suffix(" 삭제")
                        .or_else(|| label.strip_suffix(" 제거"))
                        .or_else(|| label.strip_prefix("Remove attachment "))
                        .or_else(|| label.strip_prefix("Remove "))?;
                    Path::new(file).file_name()?.to_str().map(str::to_owned)
                });
                if let Some(name) = name {
                    found.push((name, node));
                }
            } else if !node.role("AXTextArea")
                && !node.role("AXWebArea")
                && !node.named(&["Chat messages", "채팅 메시지"])
            {
                nodes.extend(node.children());
            }
        }
        nodes.is_empty().then_some(found)
    }

    fn has_attachments(&self, paths: &[PathBuf], deadline: Instant) -> bool {
        self.attachment_buttons(deadline).is_some_and(|buttons| {
            buttons.len() == paths.len()
                && paths.iter().all(|path| {
                    buttons
                        .iter()
                        .filter(|(name, _)| {
                            path.file_name().and_then(|name| name.to_str()) == Some(name)
                        })
                        .count()
                        == 1
                })
        })
    }
}

fn stage_attachments(
    folder: &Path,
    attachments: Vec<Attachment>,
) -> Result<Vec<PathBuf>, ResultCode> {
    let batch = folder
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ResultCode::InvalidAttachment)?
        .trim_start_matches('.');
    attachments
        .into_iter()
        .enumerate()
        .map(|(index, attachment)| {
            let (name, extension, bytes) = match attachment {
                Attachment::Image { name, mime, bytes } => (
                    name,
                    match mime {
                        "image/png" => "png",
                        "image/jpeg" => "jpg",
                        "image/webp" => "webp",
                        _ => return Err(ResultCode::InvalidAttachment),
                    },
                    bytes,
                ),
                Attachment::Text { name, content } => (name, "txt", content.into_bytes()),
            };
            let stem = Path::new(&name)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("file");
            // Claude writes text file paths in a quoted mention in the chat.
            let stem = stem.replace('"', "_");
            let stem = stem.trim_matches(['.', ' ']);
            let stem = &stem[..stem.floor_char_boundary(150)];
            let file = folder.join(format!("Offtab-{batch}-{}-{stem}.{extension}", index + 1));
            fs::write(&file, bytes).map_err(|_| ResultCode::InvalidAttachment)?;
            Ok(file)
        })
        .collect()
}

fn clear_owned_attachments(composer: &Composer, id: &str, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    if let Some(fresh) = Composer::find(id, deadline).filter(|fresh| {
        fresh.pid == composer.pid && fresh.window == composer.window && fresh.current(id)
    }) {
        if let Some(buttons) = fresh.attachment_buttons(deadline) {
            for (name, button) in buttons {
                if paths
                    .iter()
                    .any(|path| path.file_name().and_then(|name| name.to_str()) == Some(&name))
                    && fresh.current(id)
                {
                    unsafe {
                        AXUIElementPerformAction(
                            button.0.as_CFTypeRef(),
                            CFString::new("AXPress").as_concrete_TypeRef(),
                        );
                    }
                }
            }
        }
    }
}

fn post_key(pid: libc::pid_t, key: u16, flags: u64) -> Option<()> {
    let event = |down| {
        let raw = unsafe { CGEventCreateKeyboardEvent(ptr::null(), key, down) };
        (!raw.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(raw) })
    };
    let down = event(true)?;
    let up = event(false)?;
    for event in [down, up] {
        unsafe {
            CGEventSetFlags(event.as_CFTypeRef(), flags);
            CGEventPostToPid(pid, event.as_CFTypeRef());
        }
    }
    Some(())
}

pub fn send(
    session: &Session,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<ResultCode, ResultCode> {
    let _delivery = DELIVERY.try_lock().map_err(|_| ResultCode::Busy)?;
    autoreleasepool(|_| send_in_pool(session, text, attachments))
}

pub fn answer_choice(session: &Session, choice: &ChoiceRequest, index: usize) -> ResultCode {
    let Ok(_delivery) = DELIVERY.try_lock() else { return ResultCode::Busy };
    autoreleasepool(|_| answer_choice_in_pool(session, choice, index).unwrap_or_else(|code| code))
}

fn answer_choice_in_pool(session: &Session, choice: &ChoiceRequest, index: usize) -> Result<ResultCode, ResultCode> {
    if !trusted() {
        return Err(ResultCode::DesktopPermission);
    }
    if !unlocked() {
        return Err(ResultCode::DesktopLocked);
    }
    let id = targets::claude_desktop_id(session).ok_or(ResultCode::Unavailable)?;
    let path = session.transcript_path.as_deref().ok_or(ResultCode::Unavailable)?;
    let [question] = choice.questions.as_slice() else { return Err(ResultCode::Unavailable) };
    let answer = question.options.get(index).ok_or(ResultCode::Blocked)?;
    let mut file = File::open(path).map_err(|_| ResultCode::Unavailable)?;
    let start = file.metadata().map_err(|_| ResultCode::Unavailable)?.len();
    let target = targets::target(session).ok_or(ResultCode::Unavailable)?;
    if !matches!(targets::open(&target), OpenResult::Opened) {
        return Err(ResultCode::DesktopTarget);
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    let (composer, button) = loop {
        if !unlocked() {
            return Err(ResultCode::DesktopLocked);
        }
        if let Some(composer) = Composer::find(id, deadline) {
            if let Some(button) = choice_button(&composer, question, index, deadline) {
                break (composer, button);
            }
        }
        if Instant::now() >= deadline {
            return Err(ResultCode::DesktopTarget);
        }
        thread::sleep(POLL);
    };
    let latest = transcript::TranscriptCache::default().read(path, Provider::Claude, true)
        .map_err(|_| ResultCode::Unavailable)?;
    if latest.desktop_choice.as_ref() != Some(choice) || !file.metadata().is_ok_and(|meta| meta.len() == start) {
        return Err(ResultCode::Busy);
    }
    if !composer.current(id) || !button.enabled() {
        return Err(ResultCode::DesktopTarget);
    }
    if unsafe { AXUIElementPerformAction(button.0.as_CFTypeRef(), CFString::new("AXPress").as_concrete_TypeRef()) } != 0 {
        return Ok(ResultCode::Unknown);
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match transcript::claude_user_message_appended(&mut file, start, &session.id, answer, 0, &[]) {
            Ok(true) => return Ok(ResultCode::Sent),
            Ok(false) if Instant::now() < deadline => thread::sleep(POLL),
            _ => return Ok(ResultCode::Unknown),
        }
    }
}

fn choice_button(composer: &Composer, question: &crate::model::ChoiceQuestion, index: usize, deadline: Instant) -> Option<Element> {
    let card = find_unique(&composer.page, COMPOSER_NODES, deadline,
        |node| node.role("AXGroup") && node.named(NEXT_STEPS), |node| node.role("AXWebArea"))?;
    let frame = card.ancestors[0].as_ref()?;
    let prompt = transcript::compact(&question.question, 600);
    let _ = find_unique(frame, 60, deadline, |node| {
        node.role("AXStaticText") && ["AXValue", "AXTitle", "AXDescription"].iter().any(|attr| {
            node.string(attr).is_some_and(|value| transcript::compact(&value, 600) == prompt)
        })
    }, |node| node == &card.element)?;
    let children: Vec<_> = card.element.children().filter(|node| node.role("AXButton")).collect();
    if children.len() != question.options.len() || !children.iter().zip(&question.options).all(|(button, label)| {
        button.named(&[label])
    }) {
        return None;
    }
    children.get(index).filter(|button| button.enabled()).cloned()
}

fn send_in_pool(
    session: &Session,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<ResultCode, ResultCode> {
    if !trusted() {
        return Err(ResultCode::DesktopPermission);
    }
    if !unlocked() {
        return Err(ResultCode::DesktopLocked);
    }
    let id = targets::claude_desktop_id(session).ok_or(ResultCode::Unavailable)?;
    let path = session
        .transcript_path
        .as_deref()
        .ok_or(ResultCode::Unavailable)?;
    let mut transcript_file = File::open(path).map_err(|_| ResultCode::Unavailable)?;
    let target = targets::target(session).ok_or(ResultCode::Unavailable)?;
    if !matches!(targets::open(&target), OpenResult::Opened) {
        return Err(ResultCode::DesktopTarget);
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut composer = loop {
        if !unlocked() {
            return Err(ResultCode::DesktopLocked);
        }
        if let Some(composer) = Composer::find(id, deadline) {
            break composer;
        }
        if Instant::now() >= deadline {
            return Err(ResultCode::DesktopTarget);
        }
        thread::sleep(POLL);
    };
    if composer
        .button(STOP, Instant::now() + Duration::from_millis(500))
        .is_some()
    {
        return Err(ResultCode::Busy);
    }
    let start = transcript_file
        .metadata()
        .map_err(|_| ResultCode::Unavailable)?
        .len();
    if !composer.current(id) || !composer.prompt.enabled() {
        return Err(ResultCode::DesktopTarget);
    }
    let existing = composer
        .prompt
        .string("AXValue")
        .ok_or(ResultCode::DesktopTarget)?;
    if !matches!(existing.as_str(), "" | "\n") {
        return Err(ResultCode::DesktopDraft);
    }
    if !composer.has_attachments(&[], Instant::now() + Duration::from_millis(700)) {
        return Err(ResultCode::DesktopDraft);
    }
    let expected_images = attachments
        .iter()
        .filter(|attachment| matches!(attachment, Attachment::Image { .. }))
        .count();
    let staged = (!attachments.is_empty())
        .then(tempfile::tempdir)
        .transpose()
        .map_err(|_| ResultCode::InvalidAttachment)?;
    let mut paths = Vec::new();
    if let Some(staged) = &staged {
        if !unsafe { CGPreflightPostEventAccess() } {
            return Err(ResultCode::DesktopPermission);
        }
        paths = stage_attachments(staged.path(), attachments)?;
        if !composer.focus_current_prompt(id) {
            return Err(ResultCode::DesktopTarget);
        }
        if !composer.contains_draft("") {
            return Err(ResultCode::DesktopDraft);
        }
        let paste = FilePaste::install(&paths).ok_or(ResultCode::InvalidAttachment)?;
        if !composer.current(id) || !composer.focused_prompt() || !paste.unchanged() {
            return Err(ResultCode::DesktopTarget);
        }
        post_key(composer.pid, V_KEY, COMMAND_FLAG).ok_or(ResultCode::DesktopTarget)?;
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(fresh) = Composer::find(id, deadline).filter(|fresh| {
                fresh.pid == composer.pid
                    && fresh.window == composer.window
                    && fresh.current(id)
                    && fresh.contains_draft("")
                    && fresh.has_attachments(&paths, deadline)
            }) {
                // Pasting can replace the text area while leaving the chat window open.
                composer = fresh;
                break;
            }
            if Instant::now() >= deadline || !composer.current(id) || !paste.unchanged() {
                clear_owned_attachments(&composer, id, &paths);
                return Err(ResultCode::InvalidAttachment);
            }
            thread::sleep(POLL);
        }
    }
    let expected_files = paths
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if !composer.focus_current_prompt(id) {
        clear_owned_attachments(&composer, id, &paths);
        return Err(ResultCode::DesktopTarget);
    }
    if !composer.contains_draft("") {
        clear_owned_attachments(&composer, id, &paths);
        return Err(ResultCode::DesktopDraft);
    }
    if !text.is_empty() && !composer.prompt.set_text(text) {
        clear_owned_attachments(&composer, id, &paths);
        return Err(ResultCode::DesktopTarget);
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    let destination = loop {
        if !composer.current(id) {
            break None;
        }
        if let Some(fresh) = Composer::find(id, deadline).filter(|fresh| {
            fresh.pid == composer.pid
                && fresh.window == composer.window
                && fresh.contains_draft(text)
                && fresh.has_attachments(&paths, deadline)
        }) {
            if let Some(button) = fresh.button(SEND, deadline).filter(Element::enabled) {
                break Some((fresh, button));
            }
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(POLL);
    };
    let same_log = transcript_file
        .metadata()
        .is_ok_and(|meta| meta.len() == start);
    let Some((fresh, button)) = destination.filter(|(fresh, button)| {
        same_log
            && button.enabled()
            && fresh.current(id)
            && fresh.contains_draft(text)
            && fresh.has_attachments(&paths, Instant::now() + Duration::from_millis(500))
    }) else {
        if let Some(fresh) =
            Composer::find(id, Instant::now() + Duration::from_millis(500)).filter(|fresh| {
                fresh.pid == composer.pid
                    && fresh.window == composer.window
                    && fresh.current(id)
                    && fresh.contains_draft(text)
            })
        {
            fresh.prompt.set_text(&existing);
        }
        clear_owned_attachments(&composer, id, &paths);
        return Err(if !unlocked() {
            ResultCode::DesktopLocked
        } else {
            ResultCode::DesktopTarget
        });
    };
    let ready = || {
        button.enabled()
            && fresh.contains_draft(text)
            && fresh.has_attachments(&paths, Instant::now() + Duration::from_millis(500))
            && transcript_file
                .metadata()
                .is_ok_and(|meta| meta.len() == start)
            && fresh.current(id)
    };
    // Fall back only when no key event was posted. A slow or failed confirmation
    // can still mean Claude received the first submission.
    if fresh.activate_send_key(id, &button, &ready).is_none() {
        if !ready() {
            return Err(ResultCode::DesktopTarget);
        }
        unsafe {
            AXUIElementPerformAction(
                button.0.as_CFTypeRef(),
                CFString::new("AXPress").as_concrete_TypeRef(),
            );
        }
    }
    let deadline =
        Instant::now() + Duration::from_secs(if expected_files.is_empty() { 3 } else { 7 });
    loop {
        match transcript::claude_user_message_appended(
            &mut transcript_file,
            start,
            &session.id,
            text,
            expected_images,
            &expected_files,
        ) {
            Ok(true) => return Ok(ResultCode::Sent),
            Err(_) => return Ok(ResultCode::Unknown),
            Ok(false) if Instant::now() < deadline => thread::sleep(POLL),
            Ok(false) => return Ok(ResultCode::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_session;

    #[test]
    fn accepts_only_the_exact_claude_desktop_session_address() {
        let id = "local_77777777-7777-4777-8777-777777777777";
        assert!(is_session(&format!("https://claude.ai/epitaxy/{id}"), id));
        for url in [
            format!("https://claude.ai.evil.test/epitaxy/{id}"),
            format!("http://claude.ai/epitaxy/{id}"),
            format!("https://claude.ai/epitaxy/{id}-different"),
            format!("https://claude.ai/other/{id}"),
        ] {
            assert!(!is_session(&url, id));
        }
    }
}

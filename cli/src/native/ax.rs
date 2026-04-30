//! macOS Accessibility (AX) snapshot of Chrome's process tree.
//!
//! Native port of probe.swift's `focusedSnapshot()` — see
//! `~/Goodboy/_research/ax-mcp/probe.swift` for the Swift source-of-truth.
//!
//! Surfaces the focused element + visible popups (autofill picker,
//! save-password infobar, WebAuthn modal). Skips AXWebArea (page DOM is
//! reachable via CDP). The TCC Accessibility grant must be held by THIS
//! binary; ad-hoc codesigning keeps the grant stable across rebuilds.

#![allow(dead_code)]

#[cfg(target_os = "macos")]
mod imp {
    use accessibility_sys::{
        kAXChildrenAttribute, kAXDescriptionAttribute, kAXFocusedAttribute,
        kAXFocusedUIElementAttribute, kAXHelpAttribute, kAXMainAttribute, kAXParentAttribute,
        kAXPickAction, kAXPositionAttribute, kAXPressAction, kAXRoleAttribute,
        kAXSelectedAttribute, kAXSizeAttribute, kAXSubroleAttribute, kAXTitleAttribute,
        kAXValueAttribute, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowsAttribute,
        AXIsProcessTrusted, AXUIElementCopyActionNames, AXUIElementCopyAttributeValue,
        AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementPerformAction,
        AXUIElementRef, AXValueGetValue, AXValueRef,
    };
    use core_foundation::{
        array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef},
        base::{CFGetTypeID, CFRelease, CFTypeID, CFTypeRef, TCFType},
        boolean::{CFBoolean, CFBooleanGetTypeID, CFBooleanRef},
        string::{CFString, CFStringRef},
    };
    use core_graphics_types::geometry::{CGPoint, CGSize};
    use serde_json::{json, Map, Value};
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::time::Instant;

    const MAX_WALK_DEPTH: usize = 14;
    const MAX_LIST_DEPTH: usize = 10;
    const TEXT_TRUNCATE: usize = 140;

    /// Owning wrapper around an AXUIElementRef. AXUIElement is a CFType under
    /// the hood; we drop the ref count when we go out of scope.
    struct AxElement(AXUIElementRef);

    impl AxElement {
        unsafe fn from_create(raw: AXUIElementRef) -> Option<Self> {
            if raw.is_null() {
                None
            } else {
                Some(AxElement(raw))
            }
        }
    }

    impl Drop for AxElement {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0 as CFTypeRef) }
            }
        }
    }

    fn cfstr(name: &str) -> CFString {
        CFString::new(name)
    }

    /// Reads a string attribute. Returns "" on any failure.
    fn ax_str(el: AXUIElementRef, attr: &str) -> String {
        unsafe {
            let key = cfstr(attr);
            let mut out: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(el, key.as_concrete_TypeRef(), &mut out);
            if err != 0 || out.is_null() {
                return String::new();
            }
            // Verify it's a CFString before downcasting.
            let s_ref: CFStringRef = out as CFStringRef;
            let cf = CFString::wrap_under_create_rule(s_ref);
            cf.to_string()
        }
    }

    /// Reads a bool attribute. Returns false on any failure.
    fn ax_bool(el: AXUIElementRef, attr: &str) -> bool {
        unsafe {
            let key = cfstr(attr);
            let mut out: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(el, key.as_concrete_TypeRef(), &mut out);
            if err != 0 || out.is_null() {
                return false;
            }
            let tid: CFTypeID = CFGetTypeID(out);
            if tid != CFBooleanGetTypeID() {
                CFRelease(out);
                return false;
            }
            let b = CFBoolean::wrap_under_create_rule(out as CFBooleanRef);
            b.into()
        }
    }

    /// Reads an array of children AXUIElements. Returns empty vec on failure.
    fn ax_children(el: AXUIElementRef) -> Vec<AxElement> {
        ax_element_array(el, kAXChildrenAttribute)
    }

    /// Iterate a CFArrayRef of AXUIElementRefs into owning AxElements.
    /// Caller transfers the +1 retain on the array (we release it here).
    unsafe fn drain_element_array(arr_ref: CFArrayRef) -> Vec<AxElement> {
        let count = CFArrayGetCount(arr_ref);
        let mut result = Vec::with_capacity(count as usize);
        for i in 0..count {
            let raw_ptr = CFArrayGetValueAtIndex(arr_ref, i) as AXUIElementRef;
            if !raw_ptr.is_null() {
                // GetValueAtIndex doesn't retain; bump count so AxElement::Drop
                // can release safely.
                CFRetain(raw_ptr as CFTypeRef);
                result.push(AxElement(raw_ptr));
            }
        }
        CFRelease(arr_ref as CFTypeRef);
        result
    }

    /// Reads a CGPoint attribute (e.g. AXPosition). None on any failure.
    fn ax_point(el: AXUIElementRef, attr: &str) -> Option<CGPoint> {
        unsafe {
            let key = cfstr(attr);
            let mut out: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(el, key.as_concrete_TypeRef(), &mut out);
            if err != 0 || out.is_null() {
                return None;
            }
            let val_ref = out as AXValueRef;
            let mut p = CGPoint::new(0.0, 0.0);
            let ok = AXValueGetValue(
                val_ref,
                kAXValueTypeCGPoint,
                (&mut p) as *mut CGPoint as *mut c_void,
            );
            CFRelease(out);
            if ok {
                Some(p)
            } else {
                None
            }
        }
    }

    /// Reads a CGSize attribute (e.g. AXSize). None on any failure.
    fn ax_size(el: AXUIElementRef, attr: &str) -> Option<CGSize> {
        unsafe {
            let key = cfstr(attr);
            let mut out: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(el, key.as_concrete_TypeRef(), &mut out);
            if err != 0 || out.is_null() {
                return None;
            }
            let val_ref = out as AXValueRef;
            let mut s = CGSize::new(0.0, 0.0);
            let ok = AXValueGetValue(
                val_ref,
                kAXValueTypeCGSize,
                (&mut s) as *mut CGSize as *mut c_void,
            );
            CFRelease(out);
            if ok {
                Some(s)
            } else {
                None
            }
        }
    }

    /// Reads a single AXUIElement attribute (e.g. AXFocusedUIElement). None on failure.
    fn ax_element_attr(el: AXUIElementRef, attr: &str) -> Option<AxElement> {
        unsafe {
            let key = cfstr(attr);
            let mut out: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(el, key.as_concrete_TypeRef(), &mut out);
            if err != 0 || out.is_null() {
                return None;
            }
            Some(AxElement(out as AXUIElementRef))
        }
    }

    /// Reads an array of AXUIElement attribute (e.g. AXWindows, AXChildren).
    fn ax_element_array(el: AXUIElementRef, attr: &str) -> Vec<AxElement> {
        unsafe {
            let key = cfstr(attr);
            let mut out: CFTypeRef = std::ptr::null();
            let err = AXUIElementCopyAttributeValue(el, key.as_concrete_TypeRef(), &mut out);
            if err != 0 || out.is_null() {
                return Vec::new();
            }
            drain_element_array(out as CFArrayRef)
        }
    }

    extern "C" {
        fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    }

    fn element_json(el: AXUIElementRef) -> Map<String, Value> {
        let mut d = Map::new();
        let role = ax_str(el, kAXRoleAttribute);
        let subrole = ax_str(el, kAXSubroleAttribute);
        let title = ax_str(el, kAXTitleAttribute);
        let value = ax_str(el, kAXValueAttribute);
        let desc = ax_str(el, kAXDescriptionAttribute);
        let help = ax_str(el, kAXHelpAttribute);

        d.insert("role".into(), Value::String(role));
        if !subrole.is_empty() {
            d.insert("subrole".into(), Value::String(subrole));
        }
        if !title.is_empty() {
            d.insert("title".into(), Value::String(title));
        }
        if !value.is_empty() {
            d.insert("value".into(), Value::String(value));
        }
        if !desc.is_empty() {
            d.insert("desc".into(), Value::String(desc));
        }
        if !help.is_empty() {
            d.insert("help".into(), Value::String(help));
        }
        if ax_bool(el, kAXSelectedAttribute) {
            d.insert("selected".into(), Value::Bool(true));
        }
        if ax_bool(el, kAXFocusedAttribute) {
            d.insert("focused".into(), Value::Bool(true));
        }
        if let (Some(p), Some(s)) = (
            ax_point(el, kAXPositionAttribute),
            ax_size(el, kAXSizeAttribute),
        ) {
            d.insert("x".into(), json!(p.x as i64));
            d.insert("y".into(), json!(p.y as i64));
            d.insert("w".into(), json!(s.width as i64));
            d.insert("h".into(), json!(s.height as i64));
        }
        d
    }

    /// Popup detection: matches probe.swift verbatim.
    /// AXList in browser chrome (we already skip AXWebArea, so any AXList
    /// we encounter is browser-process UI) = popup. Chrome exposes the
    /// "Autofill" label via kAXDescriptionAttribute, NOT kAXTitleAttribute.
    fn is_popup_shape(el: AXUIElementRef) -> bool {
        let role = ax_str(el, kAXRoleAttribute);
        let subrole = ax_str(el, kAXSubroleAttribute);
        matches!(
            role.as_str(),
            "AXMenu" | "AXSheet" | "AXPopover" | "AXSystemDialog" | "AXList"
        ) || matches!(
            subrole.as_str(),
            "AXSystemDialog" | "AXFloatingWindow" | "AXSystemFloatingWindow" | "AXDialog"
        )
    }

    fn collect_list_items(
        el: AXUIElementRef,
        depth: usize,
        idx: &mut i64,
        items: &mut Vec<Value>,
    ) {
        if depth > MAX_LIST_DEPTH {
            return;
        }
        let role = ax_str(el, kAXRoleAttribute);
        if role == "AXStaticText" {
            let v = ax_str(el, kAXValueAttribute);
            let t = ax_str(el, kAXTitleAttribute);
            let text = if !v.is_empty() { v } else { t };

            // Chrome nests AXStaticText so the OUTER element matches role/title/value
            // but the SELECTED/FOCUSED flags live on the INNER child. Union flags
            // across the outer + first nested AXStaticText child.
            let mut selected = ax_bool(el, kAXSelectedAttribute) || ax_bool(el, kAXFocusedAttribute);
            for child in ax_children(el) {
                if ax_str(child.0, kAXRoleAttribute) == "AXStaticText" {
                    selected = selected
                        || ax_bool(child.0, kAXSelectedAttribute)
                        || ax_bool(child.0, kAXFocusedAttribute);
                    break;
                }
            }

            let mut entry = Map::new();
            entry.insert("index".into(), json!(*idx));
            entry.insert("text".into(), Value::String(text));
            entry.insert("selected".into(), Value::Bool(selected));
            if let (Some(p), Some(s)) = (
                ax_point(el, kAXPositionAttribute),
                ax_size(el, kAXSizeAttribute),
            ) {
                entry.insert("x".into(), json!(p.x as i64));
                entry.insert("y".into(), json!(p.y as i64));
                entry.insert("w".into(), json!(s.width as i64));
                entry.insert("h".into(), json!(s.height as i64));
            }
            items.push(Value::Object(entry));
            *idx += 1;
            return; // don't recurse — Chrome nests duplicate AXStaticText children
        }
        for c in ax_children(el) {
            collect_list_items(c.0, depth + 1, idx, items);
        }
    }

    fn find_popups(el: AXUIElementRef, depth: usize, results: &mut Vec<Map<String, Value>>) {
        if depth > MAX_WALK_DEPTH {
            return;
        }
        let role = ax_str(el, kAXRoleAttribute);
        if role == "AXWebArea" {
            return; // popups live in browser chrome, not page DOM
        }
        if is_popup_shape(el) {
            let mut entry = element_json(el);
            let mut items: Vec<Value> = Vec::new();
            let mut idx: i64 = 0;
            collect_list_items(el, 0, &mut idx, &mut items);
            if !items.is_empty() {
                entry.insert("items".into(), Value::Array(items));
            }
            results.push(entry);
            return; // don't recurse into popup
        }
        for c in ax_children(el) {
            find_popups(c.0, depth + 1, results);
        }
    }

    /// Returns true if the AB binary holds the AX TCC grant. False means the
    /// system needs to prompt; AX reads will return errors until granted.
    pub fn is_trusted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }

    /// JSON-shaped focused snapshot, identical layout to probe.swift `--focused`.
    /// `pid` is the Chrome process to inspect.
    pub fn focused_snapshot(pid: i32) -> Result<Value, String> {
        if !is_trusted() {
            return Err(
                "Accessibility permission required. Grant in System Settings → Privacy & \
                 Security → Accessibility for this binary, then retry."
                    .into(),
            );
        }

        let start = Instant::now();
        let app_raw = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
        let app = unsafe { AxElement::from_create(app_raw) }
            .ok_or_else(|| format!("AXUIElementCreateApplication returned null for pid {pid}"))?;

        // Focused element: prefer app-level AXFocusedUIElement; fall back to
        // system-wide focused element (when Chrome's popup AXWindow is key,
        // the app-level attribute returns null).
        let focused = ax_element_attr(app.0, kAXFocusedUIElementAttribute).or_else(|| {
            unsafe {
                let sys_raw = AXUIElementCreateSystemWide();
                let sys = AxElement::from_create(sys_raw)?;
                ax_element_attr(sys.0, kAXFocusedUIElementAttribute)
            }
        });
        let focused_value = match focused {
            Some(f) => Value::Object(element_json(f.0)),
            None => Value::Null,
        };

        let wins = ax_element_array(app.0, kAXWindowsAttribute);
        let win_count = wins.len();

        let mut popups_raw: Vec<Map<String, Value>> = Vec::new();
        for w in &wins {
            find_popups(w.0, 0, &mut popups_raw);
        }

        // Chrome exposes the same picker AXList from BOTH the popup AXWindow
        // and the main browser window. Dedup by (role, x, y, w, h).
        let mut seen: HashSet<String> = HashSet::new();
        let mut popups: Vec<Value> = Vec::new();
        for p in popups_raw {
            let key = format!(
                "{}|{}|{}|{}|{}",
                p.get("role").and_then(Value::as_str).unwrap_or(""),
                p.get("x").map(|v| v.to_string()).unwrap_or_default(),
                p.get("y").map(|v| v.to_string()).unwrap_or_default(),
                p.get("w").map(|v| v.to_string()).unwrap_or_default(),
                p.get("h").map(|v| v.to_string()).unwrap_or_default(),
            );
            if seen.insert(key) {
                popups.push(Value::Object(p));
            }
        }

        let elapsed_ms = start.elapsed().as_millis() as i64;
        Ok(json!({
            "pid": pid,
            "focused": focused_value,
            "popups": popups,
            "elapsed_ms": elapsed_ms,
            "windows": win_count,
        }))
    }

    /// Reads the action names supported by an AXUIElement. Empty on failure.
    fn ax_action_names(el: AXUIElementRef) -> Vec<String> {
        unsafe {
            let mut out: CFArrayRef = std::ptr::null();
            let err = AXUIElementCopyActionNames(el, &mut out);
            if err != 0 || out.is_null() {
                return Vec::new();
            }
            let count = CFArrayGetCount(out);
            let mut names = Vec::with_capacity(count as usize);
            for i in 0..count {
                let raw = CFArrayGetValueAtIndex(out, i) as CFStringRef;
                if !raw.is_null() {
                    let cf = CFString::wrap_under_get_rule(raw);
                    names.push(cf.to_string());
                }
            }
            CFRelease(out as CFTypeRef);
            names
        }
    }

    /// Walks ancestors (up to `max_hops`) looking for an element that supports
    /// `AXPress` or `AXPick`. Returns (element, action_name) or None.
    /// Picker AXStaticText items hold AXPress directly in our reproductions, but
    /// we keep the walk for future picker shapes that might wrap items.
    fn find_press_target(start: AxElement, max_hops: usize) -> Option<(AxElement, String)> {
        let mut current = Some(start);
        for _ in 0..=max_hops {
            let el = current.take()?;
            let actions = ax_action_names(el.0);
            if actions.iter().any(|a| a == kAXPressAction) {
                return Some((el, kAXPressAction.to_string()));
            }
            if actions.iter().any(|a| a == kAXPickAction) {
                return Some((el, kAXPickAction.to_string()));
            }
            current = ax_element_attr(el.0, kAXParentAttribute);
        }
        None
    }

    /// Walks Chrome's window tree and returns the AXUIElement of the AXList
    /// (or other popup-shape) at the given index. Dedup matches `find_popups` —
    /// same widget at same screen coords is the same popup.
    fn popup_ax_list_at_index(app: AXUIElementRef, index: usize) -> Option<AxElement> {
        let mut found: Vec<(AxElement, String)> = Vec::new();

        fn walk(el: AXUIElementRef, depth: usize, found: &mut Vec<(AxElement, String)>) {
            if depth > MAX_WALK_DEPTH {
                return;
            }
            let role = ax_str(el, kAXRoleAttribute);
            if role == "AXWebArea" {
                return;
            }
            if is_popup_shape(el) {
                let mut key = role.clone();
                if let (Some(p), Some(s)) = (
                    ax_point(el, kAXPositionAttribute),
                    ax_size(el, kAXSizeAttribute),
                ) {
                    key.push_str(&format!(
                        "|{}|{}|{}|{}",
                        p.x as i64, p.y as i64, s.width as i64, s.height as i64
                    ));
                }
                // Take ownership of a +1 retain so the AxElement Drop can release.
                unsafe { CFRetain(el as CFTypeRef) };
                found.push((AxElement(el), key));
                return; // don't recurse into popup
            }
            for c in ax_children(el) {
                walk(c.0, depth + 1, found);
            }
        }

        let wins = ax_element_array(app, kAXWindowsAttribute);
        for w in &wins {
            walk(w.0, 0, &mut found);
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut deduped: Vec<AxElement> = Vec::new();
        for (el, key) in found {
            if seen.insert(key) {
                deduped.push(el);
            }
        }
        if index < deduped.len() {
            // Take the requested element; the rest drop normally.
            Some(deduped.swap_remove(index))
        } else {
            None
        }
    }

    /// Collect AXStaticText leaves under a popup AXList — the picker items.
    /// Mirrors `collect_list_items`: AXStaticText only, no recurse into AXStaticText.
    fn popup_item_elements(list: AXUIElementRef) -> Vec<AxElement> {
        let mut items: Vec<AxElement> = Vec::new();
        fn walk(el: AXUIElementRef, depth: usize, items: &mut Vec<AxElement>) {
            if depth > MAX_LIST_DEPTH {
                return;
            }
            if ax_str(el, kAXRoleAttribute) == "AXStaticText" {
                unsafe { CFRetain(el as CFTypeRef) };
                items.push(AxElement(el));
                return;
            }
            for c in ax_children(el) {
                walk(c.0, depth + 1, items);
            }
        }
        walk(list, 0, &mut items);
        items
    }

    /// Press the AXStaticText item at popups[popup_idx].items[item_idx] via
    /// AXUIElementPerformAction. Returns a JSON report mirroring probe.swift's
    /// `--press` mode: target element, actions found, error code, success bit.
    ///
    /// AXPress is a Mach-IPC message into Chrome's accessibility action handler
    /// — it does NOT synthesize a mouse event. Works regardless of foreground
    /// state and sidesteps the CDP-input-trust silent-fill issue that motivates
    /// `hid_click`.
    pub fn press_popup_item(pid: i32, popup_idx: usize, item_idx: usize) -> Result<Value, String> {
        if !is_trusted() {
            return Err(
                "Accessibility permission required. Grant in System Settings → Privacy & \
                 Security → Accessibility for this binary, then retry."
                    .into(),
            );
        }

        let app_raw = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
        let app = unsafe { AxElement::from_create(app_raw) }
            .ok_or_else(|| format!("AXUIElementCreateApplication returned null for pid {pid}"))?;

        let list = popup_ax_list_at_index(app.0, popup_idx)
            .ok_or_else(|| format!("popup index {} not found", popup_idx))?;
        let popup_role = ax_str(list.0, kAXRoleAttribute);
        let popup_actions = ax_action_names(list.0);

        let items = popup_item_elements(list.0);
        let item_count = items.len();
        if item_idx >= item_count {
            return Err(format!(
                "item index {} out of range ({} items)",
                item_idx, item_count
            ));
        }
        let mut items_iter = items.into_iter();
        let item = items_iter.nth(item_idx).expect("bounds checked above");

        let item_role = ax_str(item.0, kAXRoleAttribute);
        let item_value = ax_str(item.0, kAXValueAttribute);
        let item_actions = ax_action_names(item.0);

        // Walk to the nearest element supporting AXPress/AXPick (item itself
        // qualifies in the Chrome autofill picker case).
        let target = find_press_target(item, 6);
        let mut report = json!({
            "popup_role": popup_role,
            "popup_actions": popup_actions,
            "item_count": item_count,
            "item_role": item_role,
            "item_value": item_value,
            "item_actions": item_actions,
        });

        match target {
            Some((target_el, action)) => {
                let target_role = ax_str(target_el.0, kAXRoleAttribute);
                let target_actions = ax_action_names(target_el.0);
                let action_cf = CFString::new(&action);
                let err = unsafe {
                    AXUIElementPerformAction(target_el.0, action_cf.as_concrete_TypeRef())
                };
                report["target_role"] = json!(target_role);
                report["target_actions"] = json!(target_actions);
                report["action_used"] = json!(action);
                report["error_raw"] = json!(err as i64);
                report["pressed"] = json!(err == 0);
                if err != 0 {
                    report["error"] = json!(format!("AXUIElementPerformAction returned {}", err));
                }
            }
            None => {
                report["pressed"] = json!(false);
                report["error"] = json!("No AXPress/AXPick action found on item or any ancestor (up 6 hops)");
            }
        }

        Ok(report)
    }

    /// Best-effort PID detection: scan for a Chrome process with
    /// `--remote-debugging-port=9222`. Mirrors chrome-step.mjs heuristic;
    /// avoids dragging in NSRunningApplication for now.
    pub fn detect_chrome_pid() -> Option<i32> {
        use std::process::Command;
        let out = Command::new("pgrep")
            .args(["-f", "--", "--remote-debugging-port=9222"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
                return Some(pid);
            }
        }
        None
    }

    // Suppress unused warnings for items kept for symmetry with probe.swift.
    const _UNUSED: &str = kAXMainAttribute;
    const _UNUSED2: usize = TEXT_TRUNCATE;

    // -----------------------------------------------------------------------
    // HID-tap synthetic click — the trusted-gesture path. Required because
    // CDP `Input.dispatchMouseEvent` is not treated as a real user gesture by
    // Chrome's autofill heuristics: clicking a saved-password field via CDP
    // either silently auto-fills (single credential) or does nothing visible
    // toward the AX picker. Posting a CGEvent through the HID tap goes through
    // the normal macOS input pipeline, which Chrome accepts as trusted.
    //
    // Side effect: the user's real cursor visibly moves to (x, y) for the
    // duration of the click. This is the price of trusted-gesture handling
    // and is intrinsic to the HID tap mechanism — `postToPid` does not move
    // the cursor but didn't reliably route mouse-down/up to the rendered
    // widget in our argos.co.uk picker repro.
    // -----------------------------------------------------------------------

    extern "C" {
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            mouse_type: u32,
            mouse_cursor_position: CGPoint,
            mouse_button: u32,
        ) -> *mut c_void;
        fn CGEventPost(tap: u32, event: *mut c_void);
    }

    const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
    const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
    const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
    const K_CG_HID_EVENT_TAP: u32 = 0;
    const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;

    fn post_event(mouse_type: u32, x: f64, y: f64) {
        unsafe {
            let pos = CGPoint { x, y };
            let ev = CGEventCreateMouseEvent(
                std::ptr::null(),
                mouse_type,
                pos,
                K_CG_MOUSE_BUTTON_LEFT,
            );
            if !ev.is_null() {
                CGEventPost(K_CG_HID_EVENT_TAP, ev);
                CFRelease(ev as CFTypeRef);
            }
        }
    }

    /// Synthesize a left-click at screen `(x, y)` via the HID event tap.
    /// Returns immediately after the up event posts; callers should sleep
    /// ~150-300ms before reading state if a follow-on UI (autofill picker,
    /// dialog) is expected to render.
    ///
    /// Caller's responsibility to ensure the target Chrome window is the
    /// key window before calling — typically already true after a runAB
    /// `click` / `open` sequence in normal recipes.
    pub fn hid_click(x: f64, y: f64) {
        post_event(K_CG_EVENT_MOUSE_MOVED, x, y);
        std::thread::sleep(std::time::Duration::from_millis(20));
        post_event(K_CG_EVENT_LEFT_MOUSE_DOWN, x, y);
        std::thread::sleep(std::time::Duration::from_millis(40));
        post_event(K_CG_EVENT_LEFT_MOUSE_UP, x, y);
    }
}

#[cfg(target_os = "macos")]
pub use imp::{detect_chrome_pid, focused_snapshot, hid_click, press_popup_item};

#[cfg(not(target_os = "macos"))]
pub fn focused_snapshot(_pid: i32) -> Result<serde_json::Value, String> {
    Err("AX snapshot is only available on macOS".into())
}

#[cfg(not(target_os = "macos"))]
pub fn detect_chrome_pid() -> Option<i32> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn hid_click(_x: f64, _y: f64) {}

#[cfg(not(target_os = "macos"))]
pub fn press_popup_item(
    _pid: i32,
    _popup_idx: usize,
    _item_idx: usize,
) -> Result<serde_json::Value, String> {
    Err("AX press is only available on macOS".into())
}

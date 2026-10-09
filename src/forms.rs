//! HTML forms: field state lives here (not in the DOM) so it survives re-layouts.

use std::collections::HashMap;

use ego_tree::NodeId;
use scraper::{Html, Node};
use unicode_width::UnicodeWidthStr;
use url::Url;

use crate::net::{Method, Request};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Password,
    Checkbox,
    Radio,
    Submit,
    Reset,
    Button,
    Image,
    Select,
    TextArea,
    File,
    Hidden,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub form: Option<usize>,
    pub kind: Kind,
    pub name: String,
    pub value: String,
    pub checked: bool,
    /// `(value, label)` pairs of a `<select>`.
    pub options: Vec<(String, String)>,
    pub selected: usize,
    pub size: usize,
    pub placeholder: String,
    /// Visible text of buttons.
    pub label: String,
    pub disabled: bool,
}

#[derive(Clone, Debug)]
pub struct Form {
    pub action: String,
    pub method: Method,
}

#[derive(Clone, Debug, Default)]
pub struct Forms {
    pub forms: Vec<Form>,
    pub fields: Vec<Field>,
    pub by_node: HashMap<NodeId, usize>,
}

fn text_content(node: ego_tree::NodeRef<Node>) -> String {
    let mut s = String::new();
    for n in node.descendants() {
        if let Node::Text(t) = n.value() {
            s.push_str(t);
        }
    }
    s
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Forms {
    pub fn build(html: &Html) -> Forms {
        let mut out = Forms::default();
        let mut form_by_node = HashMap::new();
        let mut form_by_id = HashMap::new();
        for node in html.tree.root().descendants() {
            let Some(e) = node.value().as_element() else { continue };
            if e.name() == "form" {
                let method = match e.attr("method") {
                    Some(m) if m.eq_ignore_ascii_case("post") => Method::Post,
                    _ => Method::Get,
                };
                form_by_node.insert(node.id(), out.forms.len());
                if let Some(id) = e.id() {
                    form_by_id.insert(id.to_string(), out.forms.len());
                }
                out.forms.push(Form { action: e.attr("action").unwrap_or("").trim().to_string(), method });
            }
        }
        for node in html.tree.root().descendants() {
            let Some(e) = node.value().as_element() else { continue };
            let name = e.name();
            if !matches!(name, "input" | "button" | "select" | "textarea") {
                continue;
            }
            let form = match e.attr("form") {
                Some(id) => form_by_id.get(id).copied(),
                None => node.ancestors().find_map(|a| form_by_node.get(&a.id()).copied()),
            };
            let attr = |a: &str| e.attr(a).unwrap_or("").to_string();
            let mut field = Field {
                form,
                kind: Kind::Text,
                name: attr("name"),
                value: attr("value"),
                checked: e.attr("checked").is_some(),
                options: Vec::new(),
                selected: 0,
                size: e.attr("size").and_then(|s| s.trim().parse().ok()).unwrap_or(20),
                placeholder: collapse(&attr("placeholder")),
                label: String::new(),
                disabled: e.attr("disabled").is_some(),
            };
            match name {
                "input" => {
                    field.kind = match attr("type").to_ascii_lowercase().as_str() {
                        "hidden" => Kind::Hidden,
                        "password" => Kind::Password,
                        "checkbox" => Kind::Checkbox,
                        "radio" => Kind::Radio,
                        "submit" => Kind::Submit,
                        "reset" => Kind::Reset,
                        "button" => Kind::Button,
                        "image" => Kind::Image,
                        "file" => Kind::File,
                        _ => Kind::Text,
                    };
                    field.label = match field.kind {
                        Kind::Submit if field.value.is_empty() => "Submit".into(),
                        Kind::Reset if field.value.is_empty() => "Reset".into(),
                        Kind::Image => e.attr("alt").map(collapse).filter(|a| !a.is_empty()).unwrap_or("Submit".into()),
                        _ => collapse(&field.value),
                    };
                }
                "button" => {
                    field.kind = match attr("type").to_ascii_lowercase().as_str() {
                        "reset" => Kind::Reset,
                        "button" => Kind::Button,
                        _ => Kind::Submit,
                    };
                    field.label = collapse(&text_content(node));
                    if field.label.is_empty() {
                        field.label = e.attr("aria-label").or(e.attr("title")).map(collapse).unwrap_or("Button".into());
                    }
                }
                "select" => {
                    field.kind = Kind::Select;
                    for opt in node.descendants() {
                        let Some(oe) = opt.value().as_element() else { continue };
                        if oe.name() != "option" {
                            continue;
                        }
                        let label = collapse(&text_content(opt));
                        let label = if label.is_empty() { oe.attr("label").unwrap_or("").to_string() } else { label };
                        let value = oe.attr("value").map(str::to_string).unwrap_or_else(|| label.clone());
                        if oe.attr("selected").is_some() {
                            field.selected = field.options.len();
                        }
                        field.options.push((value, label));
                    }
                }
                _ => {
                    field.kind = Kind::TextArea;
                    field.value = text_content(node);
                    field.size = e.attr("cols").and_then(|s| s.trim().parse().ok()).unwrap_or(30);
                }
            }
            out.by_node.insert(node.id(), out.fields.len());
            out.fields.push(field);
        }
        out
    }

    /// How a field looks inline on the page.
    pub fn display(&self, idx: usize) -> String {
        let f = &self.fields[idx];
        let width = f.size.clamp(4, 40);
        match f.kind {
            Kind::Text | Kind::Password | Kind::TextArea => {
                let shown = match f.kind {
                    Kind::Password => "*".repeat(f.value.chars().count()),
                    _ => f.value.lines().next().unwrap_or("").to_string(),
                };
                let shown = if shown.is_empty() { f.placeholder.clone() } else { shown };
                format!("[{}]", fit(&shown, width))
            }
            Kind::Checkbox => if f.checked { "[x]" } else { "[ ]" }.into(),
            Kind::Radio => if f.checked { "(*)" } else { "( )" }.into(),
            Kind::Submit | Kind::Reset | Kind::Button | Kind::Image => format!("[ {} ]", f.label),
            Kind::Select => {
                let label = f.options.get(f.selected).map(|o| o.1.as_str()).unwrap_or("");
                format!("[{label} v]")
            }
            Kind::File => "[file upload unsupported]".into(),
            Kind::Hidden => String::new(),
        }
    }

    /// Checks a radio button and unchecks the rest of its group.
    pub fn check_radio(&mut self, idx: usize) {
        let (form, name) = (self.fields[idx].form, self.fields[idx].name.clone());
        for f in &mut self.fields {
            if f.kind == Kind::Radio && f.form == form && f.name == name {
                f.checked = false;
            }
        }
        self.fields[idx].checked = true;
    }

    /// The button an implicit submission (Enter in a text field) uses.
    pub fn default_button(&self, form: usize) -> Option<usize> {
        self.fields
            .iter()
            .position(|f| f.form == Some(form) && matches!(f.kind, Kind::Submit | Kind::Image) && !f.disabled)
    }

    /// Builds the request for submitting `form`, optionally via the `submitter` button.
    pub fn submit(&self, form: usize, submitter: Option<usize>, page: &Url, base: &Url) -> Option<Request> {
        let fm = self.forms.get(form)?;
        if fm.action.to_ascii_lowercase().starts_with("javascript:") {
            return None;
        }
        let mut action = if fm.action.is_empty() { page.clone() } else { base.join(&fm.action).ok()? };
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (i, f) in self.fields.iter().enumerate() {
            if f.form != Some(form) || f.disabled {
                continue;
            }
            if f.kind == Kind::Image && submitter == Some(i) {
                let prefix = if f.name.is_empty() { String::new() } else { format!("{}.", f.name) };
                ser.append_pair(&format!("{prefix}x"), "0");
                ser.append_pair(&format!("{prefix}y"), "0");
                continue;
            }
            if f.name.is_empty() {
                continue;
            }
            match f.kind {
                Kind::Checkbox | Kind::Radio if f.checked => {
                    ser.append_pair(&f.name, if f.value.is_empty() { "on" } else { &f.value });
                }
                Kind::Submit if submitter == Some(i) => {
                    ser.append_pair(&f.name, &f.value);
                }
                Kind::Select => {
                    if let Some((v, _)) = f.options.get(f.selected) {
                        ser.append_pair(&f.name, v);
                    }
                }
                Kind::Text | Kind::Password | Kind::Hidden | Kind::TextArea => {
                    ser.append_pair(&f.name, &f.value);
                }
                _ => {}
            }
        }
        let query = ser.finish();
        action.set_fragment(None);
        match fm.method {
            Method::Get => {
                action.set_query(Some(&query));
                Some(Request::get(action))
            }
            Method::Post => Some(Request { url: action, method: Method::Post, body: Some(query) }),
        }
    }
}

/// Truncates or pads `s` to exactly `width` columns.
fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width.saturating_sub(1) && s.width() > width {
            out.push('…');
            w += 1;
            break;
        }
        out.push(c);
        w += cw;
    }
    out + &" ".repeat(width.saturating_sub(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_form_submission() {
        let html = Html::parse_document(
            r#"<form action="/search"><input name="q" value="rust tui"><input type="hidden" name="t" value="x">
            <input type="checkbox" name="c"><select name="s"><option>a<option selected value="bv">b</select>
            <input type="submit" name="go" value="Go"></form>"#,
        );
        let mut forms = Forms::build(&html);
        assert_eq!(forms.fields.len(), 5);
        forms.fields[2].checked = true;
        let page = Url::parse("https://example.org/a/b?old=1").unwrap();
        let btn = forms.default_button(0);
        let req = forms.submit(0, btn, &page, &page).unwrap();
        assert_eq!(req.url.as_str(), "https://example.org/search?q=rust+tui&t=x&c=on&s=bv&go=Go");
    }

    #[test]
    fn radio_groups_and_display() {
        let html = Html::parse_document(
            r#"<form method=post><input type=radio name=r value=1 checked><input type=radio name=r value=2>
            <input name=t size=5 placeholder="type here"><button>Send it</button></form>"#,
        );
        let mut forms = Forms::build(&html);
        forms.check_radio(1);
        assert!(!forms.fields[0].checked && forms.fields[1].checked);
        assert_eq!(forms.display(1), "(*)");
        assert_eq!(forms.display(2), "[type…]");
        assert_eq!(forms.display(3), "[ Send it ]");
        let page = Url::parse("https://example.org/p").unwrap();
        let req = forms.submit(0, None, &page, &page).unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.body.as_deref(), Some("r=2&t="));
    }
}

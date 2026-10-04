use webkit6::{javascriptcore, NavigationType};

pub(crate) const MESSAGE_HANDLER: &str = "eclipseNavigation";

pub(crate) const SCRIPT_WORLD: &str = "eclipse-navigation";

pub(crate) fn script() -> String {
    format!(
        "(function(){{var h=window.webkit&&window.webkit.messageHandlers&&\
         window.webkit.messageHandlers.{MESSAGE_HANDLER};if(!h){{return;}}\
         function post(kind,url){{var u=navigator.userActivation;\
         h.postMessage({{kind:kind,url:String(url),activation:!!(u&&u.isActive)}});}}\
         function own(target){{var b=document.querySelector('base[target]');\
         var t=(target===null?(b?b.target:''):target).toLowerCase();\
         return t===''||t==='_self'||t==='_top'||t==='_parent';}}\
         addEventListener('click',function(e){{\
         if(e.button!==0||e.ctrlKey||e.shiftKey||e.metaKey||e.altKey){{return;}}\
         var a=e.target&&e.target.closest?e.target.closest('a[href],area[href]'):null;\
         if(a&&typeof a.href==='string'&&!a.hasAttribute('download')&&\
         own(a.getAttribute('target'))){{post('link',a.href);}}}},true);\
         addEventListener('submit',function(e){{var f=e.target,s=e.submitter;\
         var o=function(n){{return s&&s.hasAttribute('form'+n);}};\
         var m=o('method')?s.formMethod:f.method;\
         var t=o('target')?s.getAttribute('formtarget'):f.getAttribute('target');\
         if(m==='get'&&own(t)){{post('form',o('action')?s.formAction:f.action);}}}},true);\
         if(window.navigation){{navigation.addEventListener('navigate',function(e){{\
         if(!e.destination.sameDocument&&\
         (e.navigationType==='push'||e.navigationType==='replace')){{\
         post('page',e.destination.url);}}}});}}}})();"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Destination {
    Page(String),
    Link(String),
    FormAction(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) destination: Destination,
    pub(crate) activation: bool,
}

fn without_fragment(url: &str) -> &str {
    url.split_once('#').map_or(url, |(base, _)| base)
}

fn without_query(url: &str) -> &str {
    let base = without_fragment(url);
    base.split_once('?').map_or(base, |(path, _)| path)
}

impl Destination {
    fn explains(&self, kind: NavigationType) -> bool {
        match self {
            Destination::Page(_) => matches!(
                kind,
                NavigationType::LinkClicked | NavigationType::FormSubmitted | NavigationType::Other
            ),
            Destination::Link(_) => kind == NavigationType::LinkClicked,
            Destination::FormAction(_) => kind == NavigationType::FormSubmitted,
        }
    }

    fn reaches(&self, url: &str) -> bool {
        match self {
            Destination::Page(page) | Destination::Link(page) => {
                without_fragment(page) == without_fragment(url)
            }
            Destination::FormAction(action) => without_query(action) == without_query(url),
        }
    }
}

impl Intent {
    pub(crate) fn from_message(message: &javascriptcore::Value) -> Option<Intent> {
        if !message.is_object() {
            return None;
        }
        let url = message
            .object_get_property("url")
            .filter(javascriptcore::Value::is_string)?
            .to_str()
            .to_string();
        let destination = match message.object_get_property("kind")?.to_str().as_str() {
            "page" => Destination::Page(url),
            "link" => Destination::Link(url),
            "form" => Destination::FormAction(url),
            _ => return None,
        };
        let activation = message
            .object_get_property("activation")
            .filter(javascriptcore::Value::is_boolean)?
            .to_boolean();
        Some(Intent {
            destination,
            activation,
        })
    }

    pub(crate) fn applies_to(&self, url: &str, kind: NavigationType, current: &str) -> bool {
        let same_document = url.contains('#') && without_fragment(url) == without_fragment(current);
        self.destination.explains(kind) && self.destination.reaches(url) && !same_document
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(destination: Destination) -> Intent {
        Intent {
            destination,
            activation: false,
        }
    }

    fn page(url: &str) -> Intent {
        report(Destination::Page(url.to_string()))
    }

    fn link(url: &str) -> Intent {
        report(Destination::Link(url.to_string()))
    }

    fn form(action: &str) -> Intent {
        report(Destination::FormAction(action.to_string()))
    }

    const FAKE_PAGE: &str = "var posted=[],listeners={},base=null;\
         var window={webkit:{messageHandlers:{}}};\
         var navigator={userActivation:{isActive:true}};\
         var document={querySelector:function(){return base;}};\
         function addEventListener(type,listener,capture){\
         listeners[type]={listener:listener,capture:capture};}\
         var navigation={addEventListener:function(type,listener){\
         listeners[type]={listener:listener,capture:false};}};window.navigation=navigation;\
         function element(attributes,properties){var e=properties||{};\
         e.hasAttribute=function(n){return n in attributes;};\
         e.getAttribute=function(n){return n in attributes?attributes[n]:null;};return e;}\
         function click(anchor,extra){var e={button:0,target:{closest:function(){\
         return anchor;}}};for(var k in extra){e[k]=extra[k];}\
         listeners.click.listener(e);}\
         function submit(form,submitter){listeners.submit.listener({target:form,\
         submitter:submitter||null});}\
         function navigate(url,type,sameDocument){listeners.navigate.listener({\
         navigationType:type,destination:{url:url,sameDocument:sameDocument}});}";

    fn evaluate(context: &javascriptcore::Context, source: &str) -> String {
        let value = context.evaluate(source);
        if let Some(exception) = context.exception() {
            panic!("{} in {source}", exception.to_str());
        }
        value
            .map(|value| value.to_str().to_string())
            .unwrap_or_default()
    }

    fn page_running_the_script() -> javascriptcore::Context {
        let context = javascriptcore::Context::new();
        evaluate(&context, FAKE_PAGE);
        evaluate(
            &context,
            &format!(
                "window.webkit.messageHandlers.{MESSAGE_HANDLER}=\
                 {{postMessage:function(m){{posted.push(m);}}}};"
            ),
        );
        evaluate(&context, &script());
        context
    }

    fn posted_after(actions: &str) -> String {
        let context = page_running_the_script();
        evaluate(&context, actions);
        evaluate(&context, "JSON.stringify(posted)")
    }

    #[test]
    fn the_script_reports_top_frame_link_clicks_get_forms_and_page_navigations() {
        assert_eq!(
            posted_after(
                "click(element({},{href:'https://www.roblox.com/a'}));\
                 click(element({target:'_TOP'},{href:'https://www.roblox.com/b'}));\
                 submit(element({},{method:'get',action:'https://www.roblox.com/search'}));\
                 submit(element({},{method:'post',action:'https://www.roblox.com/login'}),\
                 element({formmethod:'get',formaction:'x'},{formMethod:'get',\
                 formAction:'https://www.roblox.com/find'}));\
                 navigate('https://www.roblox.com/mobile-app-upgrades/buy?id=x','push',false);\
                 navigate('https://www.roblox.com/c','replace',false);\
                 navigator.userActivation.isActive=false;\
                 navigate('https://www.roblox.com/d','push',false);\
                 navigator={};\
                 navigate('https://www.roblox.com/e','push',false);"
            ),
            "[{\"kind\":\"link\",\"url\":\"https://www.roblox.com/a\",\"activation\":true},\
             {\"kind\":\"link\",\"url\":\"https://www.roblox.com/b\",\"activation\":true},\
             {\"kind\":\"form\",\"url\":\"https://www.roblox.com/search\",\"activation\":true},\
             {\"kind\":\"form\",\"url\":\"https://www.roblox.com/find\",\"activation\":true},\
             {\"kind\":\"page\",\"url\":\"https://www.roblox.com/mobile-app-upgrades/buy?id=x\",\
             \"activation\":true},\
             {\"kind\":\"page\",\"url\":\"https://www.roblox.com/c\",\"activation\":true},\
             {\"kind\":\"page\",\"url\":\"https://www.roblox.com/d\",\"activation\":false},\
             {\"kind\":\"page\",\"url\":\"https://www.roblox.com/e\",\"activation\":false}]"
        );
        assert_eq!(
            evaluate(
                &page_running_the_script(),
                "[listeners.click.capture,listeners.submit.capture].join()"
            ),
            "true,true",
            "clicks and submits are seen in the capture phase, before the page can stop them"
        );
    }

    #[test]
    fn the_script_ignores_navigations_that_leave_the_top_frame_or_stay_in_the_document() {
        assert_eq!(
            posted_after(
                "click(element({target:'_blank'},{href:'https://www.roblox.com/a'}));\
                 click(element({target:'frame'},{href:'https://www.roblox.com/a'}));\
                 click(element({download:''},{href:'https://www.roblox.com/a'}));\
                 click(element({},{href:'https://www.roblox.com/a'}),{ctrlKey:true});\
                 click(element({},{href:'https://www.roblox.com/a'}),{button:1});\
                 click(element({},{href:{baseVal:'x'}}));\
                 click(null);\
                 base=element({},{target:'_blank'});\
                 click(element({},{href:'https://www.roblox.com/a'}));\
                 submit(element({},{method:'get',action:'https://www.roblox.com/search'}));\
                 base=null;\
                 submit(element({},{method:'post',action:'https://www.roblox.com/login'}));\
                 submit(element({target:'frame'},{method:'get',action:'https://www.roblox.com/s'}));\
                 submit(element({},{method:'get',action:'https://www.roblox.com/s'}),\
                 element({formtarget:'_blank'},{}));\
                 navigate('https://www.roblox.com/a#b','push',true);\
                 navigate('https://www.roblox.com/a','reload',false);\
                 navigate('https://www.roblox.com/a','traverse',false);"
            ),
            "[]"
        );
    }

    #[test]
    fn messages_from_the_script_become_intents_and_anything_else_is_dropped() {
        let context = javascriptcore::Context::new();
        let message = |source: &str| {
            Intent::from_message(&context.evaluate(source).expect("evaluate the message"))
        };
        assert_eq!(
            message("({kind:'page',url:'https://www.roblox.com/a',activation:true})"),
            Some(Intent {
                destination: Destination::Page("https://www.roblox.com/a".to_string()),
                activation: true,
            })
        );
        assert_eq!(
            message("({kind:'link',url:'https://www.roblox.com/b',activation:true})"),
            Some(Intent {
                destination: Destination::Link("https://www.roblox.com/b".to_string()),
                activation: true,
            })
        );
        assert_eq!(
            message("({kind:'form',url:'https://www.roblox.com/search',activation:false})"),
            Some(Intent {
                destination: Destination::FormAction("https://www.roblox.com/search".to_string()),
                activation: false,
            })
        );
        for malformed in [
            "'https://www.roblox.com/a'",
            "({kind:'iframe',url:'https://www.roblox.com/a',activation:true})",
            "({kind:'page',url:7,activation:true})",
            "({kind:'page',activation:true})",
            "({kind:'page',url:'https://www.roblox.com/a',activation:1})",
        ] {
            assert_eq!(message(malformed), None, "{malformed}");
        }
    }

    #[test]
    fn a_page_intent_reaches_its_url_whatever_the_fragment() {
        let current = "https://www.roblox.com/upgrades/robux";
        let buy = "https://www.roblox.com/mobile-app-upgrades/buy?id=com.roblox.robloxmobile.\
                   premium80robux";
        let other = NavigationType::Other;
        assert!(page(buy).applies_to(buy, other, current));
        assert!(page(&format!("{buy}#top")).applies_to(buy, other, current));
        assert!(page(buy).applies_to(&format!("{buy}#top"), other, current));
        assert!(link(buy).applies_to(&format!("{buy}#top"), NavigationType::LinkClicked, current));
        assert!(!page(buy).applies_to(&format!("{buy}x"), other, current));
        assert!(!page(buy).applies_to(
            "https://www.roblox.com/mobile-app-upgrades/buy",
            other,
            current
        ));
        assert!(!page(buy).applies_to("https://web.roblox.com/", other, current));
    }

    #[test]
    fn a_form_intent_reaches_its_action_with_any_query() {
        let search = form("https://www.roblox.com/search?old=1");
        let current = "https://www.roblox.com/home";
        let submitted = NavigationType::FormSubmitted;
        assert!(search.applies_to("https://www.roblox.com/search?q=a+b", submitted, current));
        assert!(search.applies_to("https://www.roblox.com/search", submitted, current));
        assert!(!search.applies_to("https://www.roblox.com/searches?q=a", submitted, current));
    }

    #[test]
    fn each_report_explains_only_the_kinds_of_navigation_it_can_cause() {
        use NavigationType::{
            BackForward, FormResubmitted, FormSubmitted, LinkClicked, Other, Reload,
        };
        let url = "https://www.roblox.com/search?q=a";
        let current = "https://www.roblox.com/home";
        for (report, explained) in [
            (page(url), [LinkClicked, FormSubmitted, Other].as_slice()),
            (link(url), [LinkClicked].as_slice()),
            (form(url), [FormSubmitted].as_slice()),
        ] {
            for kind in [
                LinkClicked,
                FormSubmitted,
                Other,
                BackForward,
                Reload,
                FormResubmitted,
            ] {
                assert_eq!(
                    report.applies_to(url, kind, current),
                    explained.contains(&kind),
                    "{report:?} {kind:?}"
                );
            }
        }
    }

    #[test]
    fn a_jump_within_the_current_document_is_never_a_page_load() {
        let current = "https://www.roblox.com/upgrades/robux?ctx=nav#old";
        for target in [
            "https://www.roblox.com/upgrades/robux?ctx=nav#",
            "https://www.roblox.com/upgrades/robux?ctx=nav#packages",
        ] {
            assert!(
                !link(target).applies_to(target, NavigationType::LinkClicked, current),
                "{target}"
            );
        }
        assert!(
            link("https://www.roblox.com/upgrades/robux?ctx=nav").applies_to(
                "https://www.roblox.com/upgrades/robux?ctx=nav",
                NavigationType::LinkClicked,
                current
            )
        );
        assert!(
            page("https://www.roblox.com/upgrades/robux?ctx=gift#a").applies_to(
                "https://www.roblox.com/upgrades/robux?ctx=gift#a",
                NavigationType::Other,
                current
            )
        );
    }
}

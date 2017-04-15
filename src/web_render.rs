use DomNode;
use keys::KeyIter;

/// `Updater`s modify the current application state based on messages.
pub trait Updater<State, Message>: Sized {
    /// Modify the application state based on a message.
    ///
    /// `KeyIter` may be used to identify which component the message originated from.
    fn update(&self, &mut State, Message, KeyIter, &JsIo<Message>);
}

impl<F, S, M> Updater<S, M> for F
    where F: Fn(&mut S, M, KeyIter, &JsIo<M>) -> ()
{
    fn update(
        &self,
        state: &mut S,
        msg: M,
        keys: KeyIter,
        js_io: &JsIo<M>
    ) {
        (self)(state, msg, keys, js_io)
    }
}

/// `Renderer`s convert the current state to the current UI `DomNode`.
pub trait Renderer<State, Message> {

    // Note: this should really be Rendered<'a>: DomNode + 'a
    // to allow for references to bits of state, but this is
    // impossible without ATCs
    /// Type of the rendered `DomNode`
    type Rendered: DomNode<Message>;

    /// Renders a `DomNode` given the current application state
    fn render(&self, &State) -> Self::Rendered;
}

impl<F, S, R, M> Renderer<S, M> for F where F: Fn(&S) -> R, R: DomNode<M> {
    type Rendered = R;
    fn render(&self, state: &S) -> Self::Rendered {
        (self)(state)
    }
}

pub use self::private::{run, JsIo, HttpRequest, HttpResponse, HttpResult};

mod private {

    extern crate libc;

    use super::{Updater, Renderer};
    use {DomNode, DomValue, Event, KeyValue, Listener};
    use keys::Keys;
    use processors::{DomNodes, Listeners, DomNodeProcessor, ListenerProcessor};

    // This module as a whole is "use_std"-only, so these don't need to be cfg'd
    use std::ffi::{CString, CStr};
    use std::marker::PhantomData;
    use std::{mem, ptr, str};

    /// Runs the application (`updater`, `renderer`, `initial_state`) on the webpage under the element
    /// specified by `element_selector`.
    pub fn run<D, M, U, R, S>(element_selector: &str, updater: U, renderer: R, initial_state: S) -> !
        where
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>
    {
        unsafe {
            // Get initial DomNode
            let rendered = renderer.render(&initial_state);

            // Initialize the browser system
            let document = web_init();
            let root_node_element =
                document.element_from_selector(element_selector)
                    .expect(&format!(
                        "Target element of `run` was not found: {}", element_selector));

            root_node_element.remove_all_children();

            // Lives forever on the stack, referenced and mutated in callbacks
            let mut app_system = (
                rendered,
                updater,
                renderer,
                initial_state,
                VDomNode {
                    value: VNodeValue::Tag("N/A - root"),
                    keys: Keys::new(),
                    web_element: root_node_element,
                    attributes: Vec::new(),
                    listeners: Vec::new(),
                    children: Vec::new(),
                }
            );
            let app_system_mut_ptr = (&mut app_system) as *mut (D, U, R, S, VDomNode<M>);

            // Draw initial DomNode to browser
            let mut node_index = 0;
            let mut input = WebWriterAcc {
                system_ptr: app_system_mut_ptr,
                document: document,
                keys: Keys::new(),
                parent_element: &(*app_system_mut_ptr).4.web_element,
                node_level: &mut (*app_system_mut_ptr).4.children,
                node_index: &mut node_index,
            };

            (*app_system_mut_ptr).0.process_all::<WebWriter<D, M, U, R, S>>(&mut input).unwrap();

            run_main_web_loop()
        }
    }

    struct JsIoImpl<D, M, U, R, S>
        where
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M>
    {
        app_system: *mut (D, U, R, S, VDomNode<M>)
    }

    /// A single HTTP request
    #[derive(Debug, Copy, Clone)]
    pub struct HttpRequest<'a> {
        /// HTTP Method ("GET", "POST", etc.)
        pub method: &'a str,
        /// A list of HTTP header (key, value) pairs
        pub headers: &'a [(&'a str, &'a str)],
        /// Request URL
        pub url: &'a str,
        /// Request body
        pub body: &'a str,
        /// Optional request timeout in milliseconds
        pub timeout_millis: Option<u32>,
    }

    /// HTTP request `Result` indicating a possible network error or timeout
    pub type HttpResult<'a> = Result<HttpResponse<'a>, HttpError>;

    /// A single HTTP response
    #[derive(Debug, Copy, Clone)]
    pub struct HttpResponse<'a> {
        /// HTTP status
        pub status_code: u16,
        /// HTTP status text
        pub status_text: &'a str,
        /// A list of HTTP response header (key, value) pairs
        pub headers: &'a [(&'a str, &'a str)],
        /// The body of the HTTP response
        pub body: &'a str,
    }

    /// HTTP request error indicating either a network connection error or a timeout
    #[derive(Debug, Copy, Clone)]
    pub enum HttpError {
        NetworkError,
        Timeout,
    }

    /// Handler for an HTTP response
    pub trait HttpResponseHandler: 'static {
        type Message;
        fn handle<'a>(&self, HttpResult<'a>) -> Self::Message;
    }
    impl<F, Message> HttpResponseHandler for F
        where F: for<'a> Fn(HttpResult<'a>) -> Message + 'static
    {
        type Message = Message;
        fn handle<'a>(&self, response: HttpResult<'a>) -> Message {
            (self)(response)
        }
    }

    /// JavaScript IO interface
    pub trait JsIo<Message> {
        /// Issue an asynchronous HTTP request
        fn http<'b> (
            &self,
            http_request: HttpRequest<'b>,
            handler: Box<HttpResponseHandler<Message=Message>>,
        );
    }

    impl<D, M, U, R, S> JsIo<M> for JsIoImpl<D, M, U, R, S>
        where
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>
    {
        fn http<'b> (
            &self,
            http_request: HttpRequest<'b>,
            handler: Box<HttpResponseHandler<Message=M>>,
        ) {
            JsIoImpl::http(self, http_request, handler)
        }
    }

    impl<D, M, U, R, S> JsIoImpl<D, M, U, R, S>
        where
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>
    {
        fn http<'b> (
            &self,
            http_request: HttpRequest<'b>,
            handler: Box<HttpResponseHandler<Message=M>>,
        ) {
            unsafe {
                let HttpRequest { method, headers, url, body, timeout_millis } = http_request;
                let method_cstring = CString::new(method).unwrap();
                let url_cstring = CString::new(url).unwrap();
                let body_cstring = CString::new(body).unwrap();

                let header_key_cstrings: Vec<CString> =
                headers.iter().map(|header| CString::new(header.0).unwrap()).collect();

                let header_key_pointers: Vec<libc::c_int> =
                header_key_cstrings.iter().map(|cstring|
                    cstring.as_ptr() as libc::c_int).collect();

                let header_value_cstrings: Vec<CString> =
                headers.iter().map(|header| CString::new(header.1).unwrap()).collect();

                let header_value_pointers: Vec<libc::c_int> =
                header_value_cstrings.iter().map(|cstring|
                    cstring.as_ptr() as libc::c_int).collect();

                let handler_ptr = Box::into_raw(handler);

                let (handler_data_ptr, handler_vtable_ptr):
                    (*const libc::c_void, *const libc::c_void) =
                    mem::transmute(handler_ptr);

                const JS: &'static [u8] = b"\
                    var handler_fn_ptr = $0;\
                    var app_system = $1;\
                    var method = UTF8ToString($2);\
                    var url = UTF8ToString($3);\
                    var body = UTF8ToString($4);\
                    var header_len = $5;\
                    var header_key_ptr = $6;\
                    var header_value_ptr = $7;\
                    var timeout = $8;\
                    var handler_data_ptr = $9;\
                    var handler_vtable_ptr = $10;\
                    var xhr = new XMLHttpRequest();\
                    var error_fn = function(error_sig) { return function() {\
                        Runtime.dynCall('viiiiiiii', handler_fn_ptr, [error_sig, app_system, handler_data_ptr, handler_vtable_ptr, 0, 0, 0, 0]);\
                    } };\
                    xhr.addEventListener('timeout', error_fn(1));\
                    xhr.addEventListener('error', error_fn(2));\
                    xhr.addEventListener('load', function() {\
                        var stack = Runtime.stackSave();\
                        var status_code = xhr.status;\
                        var status_text = allocate(\
                            intArrayFromString(xhr.statusText), 'i8', ALLOC_STACK\
                        );\
                        var response_headers = allocate(\
                            intArrayFromString(xhr.getAllResponseHeaders()), 'i8', ALLOC_STACK\
                        );\
                        var response_body =\
                            allocate(intArrayFromString(xhr.responseText), 'i8', ALLOC_STACK);\
                        Runtime.dynCall('viiiiiiii', handler_fn_ptr, [0, app_system, handler_data_ptr, handler_vtable_ptr, status_code, status_text, response_body, response_headers]);\
                        Runtime.stackRestore(stack);\
                    });\
                    try { xhr.open(method, url, true); } catch (e) { error_fn(3); return; }\
                    for (var i = 0; i < header_len; i++) {\
                        var header_key = UTF8ToString(getValue(header_key_ptr + (i * 4), '*'));\
                        var header_value = UTF8ToString(getValue(header_value_ptr + (i * 4), '*'));\
                        xhr.setRequestHeader(header_key, header_value);\
                    }\
                    xhr.responseType = 'text';\
                    if (timeout != 0) { xhr.timeout = timeout; }\
                    xhr.send(body);\
                \0";

                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    handle_http_result::<D, M, U, R, S> as *const libc::c_void,
                    self.app_system as *const libc::c_void,
                   method_cstring.as_ptr() as libc::c_int,
                    url_cstring.as_ptr() as libc::c_int,
                    body_cstring.as_ptr() as libc::c_int,
                    header_key_pointers.len() as libc::c_int,
                    header_key_pointers.as_ptr() as *const _ as *const libc::c_char,
                    header_value_pointers.as_ptr() as *const _ as *const libc::c_char,
                    timeout_millis.unwrap_or(0) as libc::c_int,
                    handler_data_ptr,
                    handler_vtable_ptr,
                );
            }
        }
    }

    unsafe extern fn handle_http_result<D, M, U, R, S>
    (
        error_sig: libc::c_int,
        system_c_ptr: *mut libc::c_void,
        handler_data_ptr: *const libc::c_void,
        handler_vtable_ptr: *const libc::c_void,
        status_code: u16,
        status_text: *const libc::c_char,
        body: *const libc::c_char,
        headers_ptr: *const libc::c_char
    )
        where
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>,
    {
        let handler_ptr: *mut HttpResponseHandler<Message=M> =
            mem::transmute((handler_data_ptr, handler_vtable_ptr));
        let handler = Box::from_raw(handler_ptr);

        let status_text = str::from_utf8(CStr::from_ptr(status_text).to_bytes()).unwrap();

        let headers;
        let response_result = match error_sig {
            0 => {
                let headers_str = str::from_utf8(CStr::from_ptr(headers_ptr).to_bytes()).unwrap();
                headers = headers_str.split("\r\n").flat_map(|header| {
                    header.find(':').map(|split_index| {
                        let (key, value) = header.split_at(split_index);
                        (key.trim(), value[1..].trim())
                    })
                }).collect::<Vec<_>>();

                Ok(HttpResponse {
                    status_code: status_code,
                    status_text: status_text,
                    headers: &headers,
                    body: str::from_utf8(CStr::from_ptr(body).to_bytes()).unwrap(),
                })
            },

            1 => Err(HttpError::Timeout),

            2 => Err(HttpError::NetworkError),

            _ => unreachable!(),
        };

        let message = handler.handle(response_result);

        update_system::<D, M, U, R, S>(system_c_ptr, message, Keys::new());
    }

    extern "C" {
        pub fn emscripten_asm_const_int(s: *const libc::c_char, ...) -> libc::c_int;
        fn emscripten_pause_main_loop();
        fn emscripten_set_main_loop(m: extern fn(), fps: libc::c_int, infinite: libc::c_int);
    }

    type JsElementId = libc::c_int;

    #[derive(Debug)]
    struct WebElement(JsElementId);

    #[derive(Debug, Copy, Clone)]
    struct WebDocument(()); // Contains private () so that it can't be created externally

    fn web_init() -> WebDocument {
        const JS: &'static [u8] = b"\
            if('undefined'===typeof __domafic_pool){\
                console.log('Intializing __domafic_pool');\
                __domafic_pool=[];\
                __domafic_pool_free=[];\
            }\
        \0";

        unsafe {
            emscripten_asm_const_int(&JS[0] as *const _ as *const libc::c_char);
        }

        WebDocument(())
    }

    extern fn pause_main_web_loop() {
        unsafe { emscripten_pause_main_loop(); }
    }

    fn run_main_web_loop() -> ! {
        unsafe { emscripten_set_main_loop(pause_main_web_loop, 0, 1); }
        panic!("Emscripten main loop should never return")
    }

    impl WebDocument {
        fn element_from_selector(&self, selector: &str) -> Option<WebElement> {
            let id = {
                unsafe {
                    const JS: &'static [u8] = b"\
                        var elem = document.querySelector(UTF8ToString($0));\
                        if (!elem) {return -1;}\
                        var index = __domafic_pool_free.pop();\
                        if (index) { __domafic_pool[index] = elem; return index; }\
                        return __domafic_pool.push(elem) - 1;\
                    \0";
                    let selector_cstring = CString::new(selector).unwrap();
                    emscripten_asm_const_int(
                        &JS[0] as *const _ as *const libc::c_char,
                        selector_cstring.as_ptr() as libc::c_int
                    )
                }
            };
            if id < 0 { None } else { Some(WebElement(id)) }
        }

        fn create_element(&self, tagname: &str) -> Option<WebElement> {
            println!("creating element: {}", tagname);
            let id = {
                unsafe {
                    const JS: &'static [u8] = b"\
                        var elem = document.createElement(UTF8ToString($0));\
                        if (!elem) {return -1;}\
                        var index = __domafic_pool_free.pop();\
                        if (index) { __domafic_pool[index] = elem; return index; }\
                        return __domafic_pool.push(elem) - 1;\
                    \0";
                    let tagname_cstring = CString::new(tagname).unwrap();
                    emscripten_asm_const_int(
                        &JS[0] as *const _ as *const libc::c_char,
                        tagname_cstring.as_ptr() as libc::c_int
                    )
                }
            };
            if id < 0 { None } else { Some(WebElement(id)) }
        }

        fn create_text_node(&self, text: &str) -> Option<WebElement> {
            let id = {
                unsafe {
                    const JS: &'static [u8] = b"\
                        var text = document.createTextNode(UTF8ToString($0));\
                        if (!text) {return -1;}\
                        var elem = document.createElement('span');\
                        elem.appendChild(text);\
                        var index = __domafic_pool_free.pop();\
                        if (index) { __domafic_pool[index] = elem; return index; }\
                        return __domafic_pool.push(elem) - 1;\
                    \0";
                    let text_cstring = CString::new(text).unwrap();
                    emscripten_asm_const_int(
                        &JS[0] as *const _ as *const libc::c_char,
                        text_cstring.as_ptr() as libc::c_int
                    )
                }
            };
            if id < 0 { None } else { Some(WebElement(id)) }
        }
    }

    unsafe extern fn handle_listener<D, M, U, R, S>(
        listener_data_c_ptr: *const libc::c_void,
        listener_vtable_c_ptr: *const libc::c_void,
        system_c_ptr: *mut libc::c_void,

        type_str_ptr: *const libc::c_char,
        target_value_ptr: *const libc::c_char,
        client_x: libc::c_int,
        client_y: libc::c_int,
        offset_x: libc::c_int,
        offset_y: libc::c_int,
        which_keycode: libc::c_int,
        shift_key: libc::c_int,
        alt_key: libc::c_int,
        ctrl_key: libc::c_int,
        meta_key: libc::c_int,

        keys_size: libc::c_uint,
        key_1: libc::c_uint,
        key_2: libc::c_uint,
        key_3: libc::c_uint,
        key_4: libc::c_uint,
        key_5: libc::c_uint,
        key_6: libc::c_uint,
        key_7: libc::c_uint,
        key_8: libc::c_uint,
        key_9: libc::c_uint,
        key_10: libc::c_uint,
        key_11: libc::c_uint,
        key_12: libc::c_uint,
        key_13: libc::c_uint,
        key_14: libc::c_uint,
        key_15: libc::c_uint,
        key_16: libc::c_uint,
        key_17: libc::c_uint,
        key_18: libc::c_uint,
        key_19: libc::c_uint,
        key_20: libc::c_uint,
        key_21: libc::c_uint,
        key_22: libc::c_uint,
        key_23: libc::c_uint,
        key_24: libc::c_uint,
        key_25: libc::c_uint,
        key_26: libc::c_uint,
        key_27: libc::c_uint,
        key_28: libc::c_uint,
        key_29: libc::c_uint,
        key_30: libc::c_uint,
        key_31: libc::c_uint,
        key_32: libc::c_uint,
    )
        where
        (D, U, R, S): Sized,
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>

    {
        let listener_ref: &mut Listener<M> =
            mem::transmute((listener_data_c_ptr, listener_vtable_c_ptr));

        let type_str = if (type_str_ptr as usize) != 0 {
            str::from_utf8(CStr::from_ptr(type_str_ptr).to_bytes()).ok()
        } else {
            None
        };
        let target_value = if (target_value_ptr as usize) != 0 {
            str::from_utf8(CStr::from_ptr(target_value_ptr).to_bytes()).ok()
        } else {
            None
        };
        let event = Event {
            type_str: type_str,
            target_value: target_value,
            client_x: client_x as i32,
            client_y: client_y as i32,
            offset_x: offset_x as i32,
            offset_y: offset_y as i32,
            which_keycode: which_keycode as i32,
            shift_key: shift_key == 1,
            alt_key: alt_key == 1,
            ctrl_key: ctrl_key == 1,
            meta_key: meta_key == 1,
        };

        let keys = Keys {
            size: keys_size,
            stack: [
                key_1,
                key_2,
                key_3,
                key_4,
                key_5,
                key_6,
                key_7,
                key_8,
                key_9,
                key_10,
                key_11,
                key_12,
                key_13,
                key_14,
                key_15,
                key_16,
                key_17,
                key_18,
                key_19,
                key_20,
                key_21,
                key_22,
                key_23,
                key_24,
                key_25,
                key_26,
                key_27,
                key_28,
                key_29,
                key_30,
                key_31,
                key_32,
            ]
        };

        let message = listener_ref.handle_event(event);
        update_system(system_c_ptr, message, keys);
    }

    unsafe fn update_system<D, M, U, R, S>
    (
        system_c_ptr: *mut libc::c_void,
        message: M,
        keys: Keys
    )
        where
        (D, U, R, S): Sized,
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>,
    {

        let system_ptr: *mut (D, U, R, S, VDomNode<M>) = mem::transmute(system_c_ptr);
        let system_ref: &mut (D, U, R, S, VDomNode<M>) = system_ptr.as_mut().unwrap();

        let (
            ref mut rendered,
            ref mut updater,
            ref mut renderer,
            ref mut state,
            ref mut vdom_root,
        ) = *system_ref;

        // Update state
        updater.update(state, message, keys.into_iter(), &JsIoImpl { app_system: system_ptr });

        // Render new DomNode
        // TODO: fix unsafety due to possible `panic` in `render`
        ptr::drop_in_place(rendered);
        ptr::write(rendered, renderer.render(state));

        // Write new DomNode to root element
        {
            let mut node_index = 0;
            let mut input = WebWriterAcc {
                system_ptr: system_ptr,
                document: WebDocument(()),
                keys: Keys::new(),
                parent_element: &vdom_root.web_element,
                node_level: &mut vdom_root.children,
                node_index: &mut node_index,
            };
            rendered.process_all::<WebWriter<D, M, U, R, S>>(&mut input).unwrap();
        }
    }

    impl WebElement {

        #[allow(dead_code)]
        fn append(&self, child: &WebElement) {
            unsafe {
                const JS: &'static [u8] = b"\
                    __domafic_pool[$0].appendChild(__domafic_pool[$1]);\
                \0";

                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    child.0
                );
            }
        }

        fn insert(&self, index: usize, child: &WebElement) {
            let err = unsafe {
                const JS: &'static [u8] = b"\
                    var parent = __domafic_pool[$0];\
                    if ($2 > parent.children.length) { return -1; }\
                    if ($2 == parent.children.length) {\
                        parent.appendChild(__domafic_pool[$1]);\
                    } else {\
                        parent.insertBefore(__domafic_pool[$1], parent.children[$2]);\
                    }\
                    return 0;\
                \0";

                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    child.0,
                    index as libc::c_int
                )
            };

            // Must panic on error because failure to properly add/remove nodes
            // containing listeners can cause memory unsafety
            if err < 0 { panic!("Attempted to insert child DOM element out of bounds") }
        }

        fn move_child(&self, old_index: usize, new_index: usize) {
            let err = unsafe {
                const JS: &'static [u8] = b"\
                    var parent = __domafic_pool[$0];\
                    if ($1 > parent.children.length) { return -1; }\
                    if ($2 > parent.children.length) { return -1; }\
                    var element = parent.children[$1];\
                    if ($2 == parent.children.length) {\
                        parent.appendChild(element);\
                    } else {\
                        parent.insertBefore(element, parent.children[$2]);\
                    }\
                    return 0;\
                \0";

                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    old_index as libc::c_int,
                    new_index as libc::c_int
                )
            };

            // Must panic on error because failure to properly add/remove nodes
            // containing listeners can cause memory unsafety
            if err < 0 { panic!("Attempted to move child DOM element out of bounds") }
        }

        /// Requires that `listener_ptr` and `system_ptr` are valid and that
        /// `root_node_id` is a valid `WebElement` id throughout the duration of
        /// time that it is possible for this callback to be triggered.
        /// Returns an element that is a reference to the created function
        unsafe fn set_listener<D, M, U, R, S>(
            &self,
            event_name: &str,
            listener_ptr: *const Listener<M>,
            system_ptr: *mut (D, U, R, S, VDomNode<M>),
            keys: Keys,
        ) -> WebElement
            where
            (D, U, R, S): Sized, // Make sure *mut (D, U, R, S) is a thin ptr
            D: DomNode<M>,
            M: 'static,
            U: Updater<S, M>,
            R: Renderer<S, M, Rendered=D>
        {
            unsafe {
                const JS: &'static [u8] = b"\
                    var callback = function(event) {\
                        var stack = Runtime.stackSave();\
                        event = event || window.event;\
                        var typeStr = event.type ? allocate(intArrayFromString(event.type), 'i8', ALLOC_STACK) : 0;\
                        var targetValue = (event.target && event.target.value) ? allocate(intArrayFromString(event.target.value), 'i8', ALLOC_STACK) : 0;\
                        Runtime.dynCall('viiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii', $2, [$3, $4, $5,\
                        typeStr,\
                        targetValue,\
                        Math.floor(event.clientX || 0), Math.floor(event.clientY || 0),\
                        Math.floor(event.offsetX || 0), Math.floor(event.offsetY || 0),\
                        event.which || event.keyCode || 0,\
                        event.shiftKey ? 1 : 0,\
                        event.altKey ? 1 : 0,\
                        event.ctrlKey ? 1 : 0,\
                        event.metaKey ? 1 : 0,\
                        $6, $7,\
                        $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33, $34, $35, $36, $37, $38,\
                        ]);\
                        Runtime.stackRestore(stack);\
                    };\
                    __domafic_pool[$0].addEventListener(\
                        UTF8ToString($1),\
                        callback,\
                        false\
                    );\
                    var index = __domafic_pool_free.pop();\
                    if (index) { __domafic_pool[index] = callback; return index; }\
                    return __domafic_pool.push(callback) - 1;\
                \0";

                let event_name_cstring = CString::new(event_name).unwrap();
                let Keys { size: k_size, stack: k } = keys;
                let (listener_data_c_ptr, listener_vtable_c_ptr):
                    (*const libc::c_void, *const libc::c_void) =
                    mem::transmute(listener_ptr);

                WebElement(emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    event_name_cstring.as_ptr() as libc::c_int,
                    handle_listener::<D, M, U, R, S> as *const libc::c_void,
                    listener_data_c_ptr,
                    listener_vtable_c_ptr,
                    system_ptr as *const libc::c_void,
                    k_size,
                    k[0],
                    k[1],
                    k[2],
                    k[3],
                    k[4],
                    k[5],
                    k[6],
                    k[7],
                    k[8],
                    k[9],
                    k[10],
                    k[11],
                    k[12],
                    k[13],
                    k[14],
                    k[15],
                    k[16],
                    k[17],
                    k[18],
                    k[19],
                    k[20],
                    k[21],
                    k[22],
                    k[23],
                    k[24],
                    k[25],
                    k[26],
                    k[27],
                    k[28],
                    k[29],
                    k[30],
                    k[31]
                ))
            }
        }

        fn remove_listener(&self, event_name: &str, listener: &WebElement) {
            unsafe {
                const JS: &'static [u8] = b"\
                    __domafic_pool[$0].removeEventListener(\
                        UTF8ToString($1), __domafic_pool[$2]);\
                \0";
                let event_name_cstring = CString::new(event_name).unwrap();
                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    event_name_cstring.as_ptr() as libc::c_int,
                    listener.0,
                );
            }
        }

        fn remove_all_children(&self) {
            unsafe {
                const JS: &'static [u8] = b"\
                    var elem = __domafic_pool[$0];\
                    while (elem.hasChildNodes()) { elem.removeChild(elem.lastChild); }\
                \0";
                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                );
            }
        }

        #[allow(dead_code)]
        fn remove_self(&self) {
            unsafe {
                const JS: &'static [u8] = b"\
                    var elem = __domafic_pool[$0];\
                    if (elem.parentNode) { elem.parentNode.removeChild(elem); }\
                \0";
                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                );
            }
        }

        fn remove_attribute(&self, key: &str) {
            unsafe {
                const JS: &'static [u8] = b"\
                    __domafic_pool[$0][UTF8ToString($1)] = null;\
                \0";
                let key_cstring = CString::new(key).unwrap();
                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    key_cstring.as_ptr() as libc::c_int,
                );
            }
        }

        fn set_attribute(&self, key_value: &KeyValue) {
            unsafe {
                const JS: &'static [u8] = b"\
                    __domafic_pool[$0][UTF8ToString($1)] = UTF8ToString($2);\
                \0";
                let key_cstring = CString::new(key_value.0).unwrap();
                let value_str = key_value.1.as_str();
                let value_cstring = CString::new(value_str).unwrap();
                println!("key_cstring: {:?} ", key_cstring);
                println!("value_cstring: {:?} ", value_cstring);
                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                    key_cstring.as_ptr() as libc::c_int,
                    value_cstring.as_ptr() as libc::c_int
                );
            }
        }
    }

    impl Drop for WebElement {
        fn drop(&mut self) {
            unsafe {
                const JS: &'static [u8] = b"\
                    delete __domafic_pool[$0];\
                    __domafic_pool_free.push($0);\
                \0";
                emscripten_asm_const_int(
                    &JS[0] as *const _ as *const libc::c_char,
                    self.0,
                );
            }
        }
    }

    #[derive(Debug, Clone, Eq, PartialEq)]
    enum VNodeValue {
        Text(String),
        Tag(&'static str),
    }
    #[derive(Debug)]
    struct VDomNode<Message: 'static> {
        value: VNodeValue,
        keys: Keys,
        web_element: WebElement,
        attributes: Vec<KeyValue>,
        listeners: Vec<(WebElement, *const Listener<Message>, &'static str)>,
        children: VDOMLevel<Message>,
    }
    type VDOMLevel<Message: 'static> = Vec<VDomNode<Message>>;

    struct WebWriter<'a, 'n, D, M, U, R, S>(
        PhantomData<(&'a (), &'n (), D, M, U, R, S)>
    );
    struct WebWriterAcc<'n, D, M, U, R, S> where M: 'static {
        system_ptr: *mut (D, U, R, S, VDomNode<M>),
        keys: Keys,
        document: WebDocument,
        parent_element: &'n WebElement,
        node_level: &'n mut VDOMLevel<M>,
        node_index: &'n mut usize,
    }

    impl<'a, 'n, D, M, U, R, S> DomNodeProcessor<'a, M> for WebWriter<'a, 'n, D, M, U, R, S>
        where
        D: DomNode<M>,
        M: 'static,
        U: Updater<S, M>,
        R: Renderer<S, M, Rendered=D>
    {
        type Acc = WebWriterAcc<'n, D, M, U, R, S>;
        type Error = ();

        fn get_processor<T: DomNode<M>>() -> fn(&mut Self::Acc, &'a T) -> Result<(), Self::Error> {
            fn add_node<'a, 'n, T, D, M, U, R, S>(
                acc: &mut WebWriterAcc<'n, D, M, U, R, S>,
                node: &'a T) -> Result<(), ()>
                where
                T: DomNode<M>,
                D: DomNode<M>,
                M: 'static,
                U: Updater<S, M>,
                R: Renderer<S, M, Rendered=D>
            {

                let vnode_value = match node.value() {
                    DomValue::Element { tag } => VNodeValue::Tag(tag),
                    DomValue::Text(text) => VNodeValue::Text(text.to_string()),
                };

                let keys = if let Some(new_key) = node.key() {
                    acc.keys.push(new_key)
                } else {
                    acc.keys
                };

                let listeners = {
                    let mut listeners = Vec::new();
                    node.listeners().process_all::<ListenersToVec<M>>(&mut listeners)?;
                    listeners
                };

                let vnode_match_opt_index = {
                    let mut vnode_match_opt_index = None;
                    let mut trial_index = *acc.node_index;
                    while let Some(trial_vnode) = acc.node_level.get(trial_index) {
                        // Match iff "keys" and "value" are equal.
                        // Cannot match elements with lower indices than
                        // `acc.node_index`, as they are the output of prior calls to `add_node`.
                        if (trial_vnode.keys == keys) &&
                            (trial_vnode.value == vnode_value)
                        {
                            vnode_match_opt_index = Some(trial_index);
                            break;
                        } else {
                            trial_index += 1;
                        }
                    }
                    vnode_match_opt_index
                };

                if let Some(vnode_index) = vnode_match_opt_index {
                    // Modify the existing element
                    // Add new listeners, unify attributes, unify children

                    {
                        let mut vnode = &mut acc.node_level[vnode_index];

                        // Remove excess listeners
                        {
                            let mut i = 0;
                            while i < vnode.listeners.len() {
                                let do_remove = {
                                    let ref listener = vnode.listeners[i];
                                    let (ref old_element, ref old_ptr, ref old_str) = *listener;

                                    if !listeners.iter().any(|listener|
                                        *old_ptr == *listener &&
                                        *old_str == unsafe{ (**listener).event_type_handled() }
                                    ) {
                                        vnode.web_element.remove_listener(old_str, &old_element);
                                        true
                                    } else {
                                        i += 1;
                                        false
                                    }
                                };

                                if do_remove {
                                    vnode.listeners.remove(i);
                                }
                            }
                        }

                        // Add new listeners
                        for listener in listeners {
                            unsafe {
                                let event_type = (*listener).event_type_handled();
                                if !vnode.listeners.iter().any(|x|
                                        x.1 == listener &&
                                        x.2 == event_type
                                    ) {
                                    let element = vnode.web_element.set_listener(
                                        event_type,
                                        listener,
                                        acc.system_ptr,
                                        keys
                                    );
                                    vnode.listeners.push((element, listener, event_type));
                                }
                            }
                        }

                        // Remove excess attributes
                        {
                            let mut i = 0;
                            while i < vnode.attributes.len() {
                                let do_remove = {
                                    let ref old_attribute = vnode.attributes[i];
                                    if !node.attributes().any(|attr| *attr == *old_attribute) {
                                        //vnode.web_element.remove_attribute(old_attribute.0);
                                        true
                                    } else {
                                        false
                                    }
                                };

                                if do_remove {
                                    vnode.attributes.remove(i);
                                } else {
                                    i += 1;
                                }
                            }
                        }

                        // Add new attributes
                        for new_attribute in node.attributes() {
                            if !vnode.attributes.contains(new_attribute) {
                                vnode.web_element.set_attribute(new_attribute);
                                vnode.attributes.push(new_attribute.clone());
                            }
                        }

                        // To the children!
                        let mut child_node_index = 0;
                        {
                            let mut new_acc = WebWriterAcc {
                                system_ptr: acc.system_ptr,
                                keys: keys,
                                document: acc.document,
                                parent_element: &vnode.web_element,
                                node_level: &mut vnode.children,
                                node_index: &mut child_node_index,
                            };
                            node.children().process_all::<WebWriter<D, M, U, R, S>>(&mut new_acc)?;
                        }
                        // Remove DOM elements left over from the last render that weren't repurposed
                        while child_node_index < vnode.children.len() {
                            let unused_dom_element = vnode.children.pop().unwrap();
                            unused_dom_element.web_element.remove_self();
                        }
                    }

                    // Move the element if the new index is different from the old one
                    if *acc.node_index != vnode_index {
                        acc.parent_element.move_child(vnode_index, *acc.node_index);
                        let old_vnode = acc.node_level.remove(vnode_index);
                        acc.node_level.insert(*acc.node_index, old_vnode);
                    }
                } else {
                    // Construct as a new element

                    let html_element = match node.value() {
                        DomValue::Element { tag } => {
                            acc.document.create_element(tag).unwrap()},
                        DomValue::Text(text) =>
                            acc.document.create_text_node(text).unwrap(),
                    };

                    let mut listeners_with_metadata = Vec::new();
                    for listener in listeners {
                        unsafe {
                            let event_type = (*listener).event_type_handled();
                            let element = html_element.set_listener(
                                event_type,
                                listener,
                                acc.system_ptr,
                                keys
                            );
                            listeners_with_metadata.push((element, listener, event_type));
                        }
                    }

                    let mut vnode_attributes = Vec::new();
                    for attr in node.attributes() {
                        html_element.set_attribute(attr);
                        vnode_attributes.push(attr.clone());
                    }

                    let mut vnode = VDomNode {
                        value: vnode_value,
                        keys: keys,
                        web_element: html_element,
                        attributes: vnode_attributes,
                        listeners: listeners_with_metadata,
                        children: Vec::new(),
                    };

                    let mut child_node_index = 0;
                    {
                        let mut new_acc = WebWriterAcc {
                            system_ptr: acc.system_ptr,
                            keys: keys,
                            document: acc.document,
                            parent_element: &vnode.web_element,
                            node_level: &mut vnode.children,
                            node_index: &mut child_node_index,
                        };
                        node.children().process_all::<WebWriter<D, M, U, R, S>>(&mut new_acc)?;
                    }
                    // Remove DOM elements left over from the last render that weren't repurposed
                    while child_node_index < vnode.children.len() {
                        let unused_dom_element = vnode.children.pop().unwrap();
                        unused_dom_element.web_element.remove_self();
                    }

                    acc.parent_element.insert(*acc.node_index, &vnode.web_element);
                    acc.node_level.insert(*acc.node_index, vnode);
                }

                *acc.node_index += 1;
                Ok(())
            }

            add_node
        }
    }

    struct ListenersToVec<Message: 'static>(PhantomData<Message>);
    impl<'a, M: 'static> ListenerProcessor<'a, M> for ListenersToVec<M> {
        type Acc = Vec<*const Listener<M>>;
        type Error = ();

        fn get_processor<L: Listener<M>>() -> fn(&mut Self::Acc, &'a L) -> Result<(), Self::Error> {
            fn add_listener_to_vec<M, L: Listener<M>>(
                vec: &mut Vec<*const Listener<M>>,
                listener: &L) -> Result<(), ()>
            {
                vec.push(
                    // Extend the lifetime of the listener (we know it's valid until at least the
                    // next callback) and convert it to a *const
                    unsafe { mem::transmute(listener as &Listener<M>) }
                );
                Ok(())
            }
            add_listener_to_vec
        }
    }
}


/// set title of the document
pub fn set_title(title: &str) {
    extern crate libc;
    use std::ffi::CString;
    use web_render::private::emscripten_asm_const_int;
    unsafe {
        const JS: &'static [u8] = b"\
            document.title = [UTF8ToString($0)];\
        \0";
        let title_cstring = CString::new(title).unwrap();
        emscripten_asm_const_int(
            &JS[0] as *const _ as *const libc::c_char,
            title_cstring.as_ptr() as libc::c_int,
        );
    }
}

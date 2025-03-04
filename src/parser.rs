use self::model::{Multipart, RequestTarget, WithDefault};
pub use crate::scanner::Scanner;
use crate::{
    error::{ErrorWithPartial, ParseError, ParseErrorDetails},
    model,
    model::{
        CommentKind, DataSource, DispositionField, FileParseResult, Header, HttpRestFile,
        HttpRestFileExtension, PartialRequest, RequestBody, RequestLine, RequestSettings,
        ResponseHandler, SaveResponse, SettingsEntry, UrlEncodedParam,
    },
    scanner::{LineIterator, WS_CHARS},
};
pub use http::Uri;
use regex::Regex;
use std::{fs, str::FromStr, collections::HashMap};

pub const REQUEST_SEPARATOR: &str = "###";
pub const META_COMMENT_SLASH: &str = "//";
pub const META_COMMENT_TAG: &str = "#";
pub const DEFAULT_MULTIPART_BOUNDARY: &str = "--boundary--";

pub struct Parser {}

type ParseResult<T> = Result<(T, Vec<ParseErrorDetails>), ParseErrorDetails>;

impl Parser {
    pub const REST_FILE_EXTENSIONS: [&str; 2] = ["http", "rest"];

    #[allow(dead_code)]
    pub fn has_valid_extension<T: AsRef<std::path::Path>>(path: &T) -> bool {
        match path.as_ref().extension() {
            Some(extension) => Parser::REST_FILE_EXTENSIONS.contains(&extension.to_str().unwrap()),
            _ => false,
        }
    }

    /// Parse the contents of a file into a `model::HttpRestFile`
    /// # Arguments
    /// * `path` - path to a .http or .rest file
    pub fn parse_file(path: &std::path::Path) -> Result<model::HttpRestFile, ParseError> {
        if let Ok(content) = fs::read_to_string(path) {
            let result = Parser::parse(&content, true);
            Ok(HttpRestFile {
                requests: result.requests,
                errs: result.errs,
                path: Box::new(path.to_owned()),
                extension: HttpRestFileExtension::from_path(path),
            })
        } else {
            Err(ParseError::CouldNotReadRequestFile(path.to_owned()))
        }
    }

    /// Parse the contents of a request file as string into multiple requests within a
    /// `model::FileParseResult`. This model contains all parsed requests as well as errors
    /// encountered during parsing.
    /// # Arguments
    /// * `string` - string to parse
    /// * `print_errors` - if set to true prints errors to the console
    pub fn parse(string: &str, print_errors: bool) -> model::FileParseResult {
        let mut scanner = Scanner::new(string);

        let mut requests: Vec<model::Request> = Vec::new();
        let mut errs: Vec<ErrorWithPartial> = Vec::new();

        loop {
            scanner.skip_empty_lines_and_ws();

            if scanner.is_done() {
                break;
            }
            match Parser::parse_request(&mut scanner) {
                Ok(request) => {
                    requests.push(request);
                }
                Err(err_with_partial) => {
                    errs.push(err_with_partial);
                }
            }
            scanner.skip_empty_lines();
            scanner.skip_ws();

            if scanner.is_done() {
                break;
            }

            // go to next ### that should start a request
            while let Some(line) = scanner.peek_line() {
                if line.trim_start().starts_with(REQUEST_SEPARATOR) {
                    break;
                } else {
                    scanner.skip_to_next_line();
                }
            }

            scanner.skip_empty_lines();
            scanner.skip_ws();

            if scanner.is_done() {
                break;
            }
        }

        if !errs.is_empty() && print_errors {
            eprintln!("{}", Parser::get_pretty_print_errs(&scanner, errs.iter()));
        }
        FileParseResult { requests, errs }
    }

    /// Parse a single request either until no further lines are present or a `REQUEST_SEPARATOR`
    /// is encountered
    pub fn parse_request(scanner: &mut Scanner) -> Result<model::Request, ErrorWithPartial> {
        let mut comments = Vec::new();
        let mut name: Option<String> = None;
        let mut parse_errs: Vec<ParseErrorDetails> = Vec::new();
        let mut settings = RequestSettings::default();
        let mut pre_request_script: Option<model::PreRequestScript> = None;

        scanner.skip_empty_lines();

        loop {
            // preq-request-scrip
            if scanner.peek().map_or(false, |c| c == &'<') {
                if let Ok(result) = Parser::parse_pre_request_script(scanner) {
                    pre_request_script = result;
                };
                continue;
            }
            match Parser::parse_meta_comment_line(scanner) {
                Some(Ok(SettingsEntry::NameEntry(entry_name))) => {
                    if !entry_name.is_empty() {
                        name = Some(entry_name);
                    }
                    continue;
                }
                Some(Ok(entry)) => {
                    settings.set_entry(&entry);
                    continue;
                }
                Some(Err(parse_error)) => {
                    parse_errs.push(parse_error);
                }
                None => (), // ignore
            }

            match Parser::parse_comment(scanner) {
                Ok(Some(comment_node)) => {
                    comments.push(comment_node);
                }
                Ok(None) => {
                    break;
                }
                Err(parse_error) => {
                    parse_errs.push(parse_error);
                    break;
                }
            }
        }

        // we only found comments and no request, in this case no request is present
        if scanner.is_done() {
            parse_errs.push(ParseErrorDetails {
                error: ParseError::MissingRequestTargetLine,
                details: None,
                start_pos: Some(scanner.get_pos().cursor),
                end_pos: None,
            });
            return Err(ErrorWithPartial {
                partial_request: PartialRequest {
                    name,
                    comments,
                    settings,
                    request_line: None,
                    body: None,
                    pre_request_script,
                    save_response: None,
                    headers: None,
                    response_handler: None,
                },
                details: parse_errs,
            });
        }

        // if no name has been found with meta tag @name=, set name from a comment starting with
        // '###' if there is any
        if name.is_none() {
            if let Some(position) = comments
                .iter()
                .position(|c| c.kind == CommentKind::RequestSeparator)
            {
                let comment = comments.remove(position).value.trim().to_string();
                if !comment.is_empty() {
                    name = Some(comment);
                };
            }
        }

        let request_line: Option<RequestLine> = match Parser::parse_request_line(scanner) {
            Ok((mut request_line, errs)) => {
                parse_errs.extend(errs);
                if pre_request_script.as_ref().is_some_and(|prs| prs.to_string().contains("request.variables.set")) {
                    lazy_static::lazy_static! {
                        static ref VAR_SET: Regex = Regex::new(r#"request\.variables\.set."(?<key>\w+)", "(?<value>\w+)""#).unwrap();
                        static ref HANDLE_BARS: Regex = Regex::new(r"\{\{(\w+)\}\}").unwrap();
                    }

                    let mut kv: HashMap<String, String> = HashMap::new();

                    for captures in VAR_SET.captures_iter(&pre_request_script.clone().unwrap().to_string()) {
                        let capture = |index| {
                            captures.get(index).map(|c| c.as_str().to_string())
                        };

                        println!("{captures:?}");

                        if let (Some(k), Some(v)) = (capture(1), capture(2)) {
                            kv.entry(k).or_insert(v);
                        }
                    }

                    match request_line.target.clone() {
                        RequestTarget::Absolute { uri } => {
                            let mut new_uri = uri.clone();

                            for captures in HANDLE_BARS.captures_iter(&uri) {
                                let capture = |index| {
                                    captures.get(index).map(|c| c.as_str().to_string())
                                };

                                if let Some(var_name) = capture(1) {
                                    if let Some(var) = kv.get(&var_name) {
                                        new_uri = new_uri.
                                            replace(&capture(1).unwrap(), var).
                                            replace("{", "").
                                            replace("}", "");
                                    }
                                }
                            }

                            request_line.target = RequestTarget::Absolute { uri: new_uri };
                        },
                        _ => {}
                    }
                }
                Some(request_line)
            }
            Err(parse_error) => {
                parse_errs.push(parse_error);
                None
            }
        };

        // end of request reached?
        {
            let peek_line = scanner.peek_line();
            if peek_line.is_some() && peek_line.unwrap().trim().starts_with(REQUEST_SEPARATOR) {
                if let Some(request_line) = request_line {
                    let request_node = model::Request {
                        name,
                        comments,
                        settings,
                        pre_request_script,
                        request_line,
                        // no headers nor body parsed
                        headers: vec![],
                        body: RequestBody::None,
                        response_handler: None,
                        save_response: None,
                    };
                    return Ok(request_node);
                } else {
                    return Err(ErrorWithPartial {
                        partial_request: PartialRequest {
                            name,
                            comments,
                            settings,
                            response_handler: None,
                            pre_request_script: None,
                            request_line: None,
                            headers: None,
                            save_response: None,
                            body: None,
                        },
                        details: parse_errs,
                    });
                }
            }
        }

        let headers = match Parser::parse_headers(scanner) {
            Ok(headers) => headers,
            Err(parse_err) => {
                parse_errs.push(parse_err);
                return Err(ErrorWithPartial {
                    partial_request: PartialRequest {
                        name,
                        comments,
                        settings,
                        pre_request_script,
                        request_line,
                        headers: None,
                        body: None,
                        response_handler: None,
                        save_response: None,
                    },
                    details: parse_errs,
                });
            }
        };

        scanner.skip_empty_lines();

        let (body, body_errs) = match Parser::parse_body(scanner, &headers) {
            Ok(body) => (body, Vec::<ParseErrorDetails>::new()),
            Err((body, errs)) => (body, errs),
        };

        if !body_errs.is_empty() {
            parse_errs.extend(body_errs.clone());
        }

        let response_handler = match Parser::parse_response_handler(scanner) {
            Ok(result) => result,
            Err(err) => {
                parse_errs.push(err);
                return Err(ErrorWithPartial {
                    partial_request: PartialRequest {
                        name,
                        comments,
                        settings,
                        pre_request_script,
                        request_line,
                        headers: Some(headers),
                        body: Some(body),
                        response_handler: None,
                        save_response: None,
                    },
                    details: parse_errs,
                });
            }
        };

        scanner.skip_empty_lines();

        let save_response = match Parser::parse_redirect(scanner) {
            Ok(result) => result,
            Err(err) => {
                parse_errs.push(err);
                return Err(ErrorWithPartial {
                    partial_request: PartialRequest {
                        name,
                        comments,
                        settings,
                        pre_request_script,
                        request_line,
                        headers: Some(headers),
                        body: Some(body),
                        response_handler,
                        save_response: None,
                    },
                    details: parse_errs,
                });
            }
        };
        scanner.skip_empty_lines();

        if !parse_errs.is_empty() {
            return Err(ErrorWithPartial {
                partial_request: PartialRequest {
                    name,
                    comments,
                    settings,
                    pre_request_script,
                    request_line,
                    headers: Some(headers),
                    body: Some(body),
                    response_handler,
                    save_response,
                },
                details: parse_errs,
            });
        }

        let mut request_node = model::Request {
            name,
            comments,
            // we can unwrap as there were errors and we would have returned above
            request_line: request_line.unwrap(),
            headers,
            body,
            settings,
            pre_request_script,
            response_handler,
            save_response,
        };

        // if no name set we use the first comment as name
        // Only do this for comments not containing meta sign @ as these specify the request
        // settings
        if request_node.name.is_none() && !request_node.comments.is_empty() {
            let name_pos = request_node
                .comments
                .iter()
                .position(|com| !com.value.contains('@'));
            if let Some(name_pos) = name_pos {
                let name_comment = request_node.comments.remove(name_pos);
                request_node.name = Some(name_comment.value);
            }
        }
        Ok(request_node)
    }

    /// Get string for printing errors to the console
    fn get_pretty_print_errs<'a, T>(scanner: &Scanner, errs: T) -> String
    where
        T: Iterator<Item = &'a ErrorWithPartial>,
    {
        errs.map(|err| &err.details)
            .flatten()
            .map(|err| Parser::pretty_err_string(scanner, err))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", "-".repeat(50)))
    }

    fn pretty_err_string(scanner: &Scanner, err_details: &ParseErrorDetails) -> String {
        let mut result = String::new();
        result.push_str(&format!("Error: {}\n", err_details.error));
        if err_details.start_pos.is_some() {
            let error_context =
                scanner.get_error_context(err_details.start_pos.unwrap(), err_details.end_pos);
            result.push_str(&format!(
                "Position: {}:{}\n",
                error_context.line, error_context.column
            ));
            result.push_str(&error_context.context);
        }
        result
    }

    /// Parses the meta comment line that contains a name.
    /// Assumes the comment characters ('//' or '#') for a comment have been stripped away
    fn parse_meta_name(scanner: &mut Scanner) -> Result<Option<String>, ParseErrorDetails> {
        scanner.skip_ws();

        let name_regex = "\\s*@name\\s*=\\s*(.*)";
        if let Ok(Some(captures)) = scanner.match_regex_forward(name_regex) {
            let name = captures.first().unwrap().trim().to_string();
            Ok(Some(name))
        } else {
            Ok(None)
        }
    }

    /// Match a comment line after '###', '//' or '##' has been stripped from it
    fn parse_comment_line(
        scanner: &mut Scanner,
        kind: CommentKind,
    ) -> Result<Option<model::Comment>, ParseErrorDetails> {
        scanner.skip_ws();
        match scanner.seek_return(&'\n') {
            Ok(value) => Ok(Some(model::Comment { value, kind })),
            Err(_) => {
                let position = scanner.get_pos().cursor;
                let err_details = ParseErrorDetails::new_with_position(
                    ParseError::MissingRequestTargetLine,
                    (position, None),
                );
                Err(err_details)
            }
        }
    }
    /// match a comment line after '###', '//' or '##' has been stripped from it
    fn parse_meta_comment_line(
        scanner: &mut Scanner,
    ) -> Option<Result<SettingsEntry, ParseErrorDetails>> {
        scanner.skip_ws();

        let peek_line = scanner.peek_line();

        #[allow(clippy::question_mark)]
        if peek_line.is_none() {
            return None;
        }

        let mut line_scanner = Scanner::new(&peek_line.unwrap());
        line_scanner.skip_ws();

        if line_scanner.match_str_forward(META_COMMENT_SLASH)
            || line_scanner.match_str_forward(META_COMMENT_TAG)
        {
            if let Ok(Some(name)) = Parser::parse_meta_name(&mut line_scanner) {
                scanner.skip_to_next_line();
                if !name.is_empty() {
                    return Some(Ok(SettingsEntry::NameEntry(name)));
                } else {
                    return None;
                }
            }
            let line = line_scanner.peek_line();
            #[allow(clippy::question_mark)]
            if line.is_none() {
                return None;
            }

            let result: Option<Result<SettingsEntry, ParseErrorDetails>> =
                match line.unwrap().trim() {
                    "@no-cookie-jar" => Some(Ok(SettingsEntry::NoCookieJar)),
                    "@no-redirect" => Some(Ok(SettingsEntry::NoRedirect)),
                    "@no-log" => Some(Ok(SettingsEntry::NoLog)),
                    // Non matching meta comment lines are taken as regular comments
                    _ => None,
                };

            if result.is_some() {
                scanner.skip_to_next_line();
            }

            return result;
        }

        None
    }

    /// Parse pre request scripts, which are either a path to a javascript file or blocks of text containing javascript code within '{% %}' blocks
    /// The full script is parsed as a single string if '{% %}' blocks are present otherwise a path is parsed.
    /// See also the `parse_response_handler` which parses similarly code that handles a response.
    fn parse_pre_request_script(
        scanner: &mut Scanner,
    ) -> Result<Option<model::PreRequestScript>, ParseErrorDetails> {
        if !scanner.take(&'<') {
            return Ok(None);
        };
        let start_pos = scanner.get_pos();
        scanner.skip_ws();
        if !scanner.match_str_forward("{%") {
            // if no starting script is found then a handler script should be presnet
            let line = scanner.get_line_and_advance();
            if line.is_none() {
                let details = ParseErrorDetails {
                    error: ParseError::MissingPreRequestScript,
                    details: Some("When a '<' character is encountered before the request target line you can either specify a path to a file whose content will be inserted".to_string()),
                    start_pos: Some(start_pos.cursor),
                    end_pos: Some(scanner.get_cursor()),
                };

                return Err(details);
            }
            return Ok(Some(model::PreRequestScript::FromFilepath(
                line.unwrap().trim().to_string(),
            )));
        }

        let mut found: bool = false;
        let mut lines: Vec<String> = Vec::new();
        loop {
            if let Ok(Some(result)) = scanner.match_regex_forward("(.*)%}") {
                if result.len() == 1 {
                    lines.push(result[0].to_string());
                    found = true;
                    break;
                } else {
                    let details = ParseErrorDetails::new_with_position(
                        ParseError::MissingPreRequestScriptClose,
                        (start_pos.cursor, Some(scanner.get_cursor())),
                    );
                    return Err(details);
                }
            } else {
                let line = scanner.get_line_and_advance();
                if line.is_none() {
                    break;
                }

                lines.push(line.unwrap());
            }
        }

        if !found {
            let details = ParseErrorDetails::new_with_position(
                ParseError::MissingPreRequestScriptClose,
                (start_pos.cursor, Some(scanner.get_cursor())),
            );
            return Err(details);
        }
        scanner.skip_to_next_line();
        Ok(Some(model::PreRequestScript::Script(lines.join("\n"))))
    }
    // @TODO: create a macro that generates a match statement for each enum variant
    fn match_request_method(str: &str) -> model::HttpMethod {
        // if not one of the well known methods then it is a custom method
        model::HttpMethod::new(str)
    }

    /// Parse a request line of the form '[method required-whitespace] request-target [required-whitespace http-version]'
    fn parse_request_line(scanner: &mut Scanner) -> ParseResult<model::RequestLine> {
        let mut line = match scanner.get_line_and_advance() {
            Some(line) => line,
            _ => String::new(),
        };

        let line_start = scanner.get_pos();
        // request line can be split over multiple lines but all lines following need to be
        // indented
        let line_iterator: LineIterator = scanner.iter_at_pos();

        let (indented_lines, line_end): (Vec<String>, usize) =
            line_iterator.take_while_peek(|line| {
                !line.is_empty() && WS_CHARS.contains(&line.chars().next().unwrap())
            });

        scanner.set_pos(line_end);

        if !indented_lines.is_empty() {
            line.push_str(
                &indented_lines
                    .iter()
                    .map(|l| l.trim().to_owned())
                    .collect::<Vec<String>>()
                    .join(""),
            );
        }

        let line_scanner = Scanner::new(&line);
        let tokens: Vec<String> = line_scanner.get_tokens();

        // It can be that the request line is missing but there are still headers
        if tokens.len() >= 2 && tokens[0].contains(':') {
            return Err(ParseErrorDetails {
                error: ParseError::MissingRequestTargetLine,
                details: None,
                start_pos: Some(line_start.cursor),
                end_pos: None,
            });
        }

        let (request_line, err): (model::RequestLine, Option<ParseErrorDetails>) = match &tokens[..]
        {
            [target_str] => (
                model::RequestLine {
                    target: RequestTarget::from(&target_str[..]),
                    method: model::WithDefault::default(),
                    http_version: model::WithDefault::default(),
                },
                None,
            ),
            [method, target_str] => (
                model::RequestLine {
                    target: RequestTarget::from(&target_str[..]),
                    method: WithDefault::Some(Parser::match_request_method(method)),
                    http_version: WithDefault::default(),
                },
                None,
            ),

            [method, target_str, http_version_str] => {
                let result = model::HttpVersion::from_str(http_version_str);
                let (http_version, http_version_err) = match result {
                    Ok(version) => (WithDefault::Some(version), None),
                    Err(err) => (WithDefault::default(), Some(err)),
                };

                let line_end = line_start.cursor + tokens.len();
                (
                    model::RequestLine {
                        target: RequestTarget::from(&target_str[..]),
                        method: WithDefault::Some(Parser::match_request_method(method)),
                        http_version,
                    },
                    http_version_err.map(|err| {
                        ParseErrorDetails::new_with_position(
                            err,
                            (line_start.cursor, Some(line_end)),
                        )
                    }),
                )
            }
            //
            [] => {
                return Err(ParseErrorDetails {
                    error: ParseError::MissingRequestTargetLine,
                    details: None,
                    start_pos: Some(line_start.cursor),
                    end_pos: None,
                });
            } // on a request line only method, target and http_version should be present
            [method, target_str, http_version_str, ..] => {
                let result = model::HttpVersion::from_str(http_version_str);
                let http_version = match result {
                    Ok(version) => Some(version),
                    Err(_) => None,
                };

                let error_details = ParseErrorDetails::new_with_position(
                    ParseError::TooManyElementsOnRequestLine(tokens[3..].join(",")),
                    (line_start.cursor, Some(line_end)),
                );

                (
                    model::RequestLine {
                        target: RequestTarget::from(&target_str[..]),
                        method: WithDefault::Some(Parser::match_request_method(method)),
                        http_version: WithDefault::from(http_version),
                    },
                    Some(error_details),
                )
            }
        };

        let mut errs: Vec<ParseErrorDetails> = Vec::new();
        if let Some(err) = err {
            errs.push(err);
        }
        Ok((request_line, errs))
    }

    /// Parse a regular comment either starts with '###' or with '//' or '#'
    /// Both '//' and '#' comments may contain meta information, in this case they are not parsed
    /// as regular comments. If a '###' comment occurs alone without any other comments, then it
    /// signifies the name of a request and will be transformed afterwards and not taken as regular
    /// comment.
    /// Note that '###' can also be a request separator
    fn parse_comment(scanner: &mut Scanner) -> Result<Option<model::Comment>, ParseErrorDetails> {
        scanner.skip_empty_lines();
        // comments can be indented
        scanner.skip_ws();

        if scanner.match_str_forward(CommentKind::RequestSeparator.string_repr()) {
            return Parser::parse_comment_line(scanner, CommentKind::RequestSeparator);
        }

        if scanner.match_str_forward(CommentKind::DoubleSlash.string_repr()) {
            return Parser::parse_comment_line(scanner, CommentKind::DoubleSlash);
        }

        // @TODO: is single comment allowed if not a name comment line?
        if scanner.match_str_forward(CommentKind::SingleTag.string_repr()) {
            return Parser::parse_comment_line(scanner, CommentKind::SingleTag);
        }

        Ok(None)
    }

    /// Parse http headers, they can either belong to a request or each multipart part can also
    /// contain headers. This function is used to parse both cases.
    fn parse_headers(scanner: &mut Scanner) -> Result<Vec<model::Header>, ParseErrorDetails> {
        let mut headers: Vec<model::Header> = Vec::new();

        let header_regex = regex::Regex::from_str("^([^:]+):\\s*(.+)\\s*").unwrap();

        loop {
            if scanner.is_done() {
                return Ok(headers);
            }

            // newline after requestline and headers ends header section
            if let Some(&'\n') = scanner.peek() {
                return Ok(headers);
            }

            let line = scanner.get_line_and_advance().unwrap();
            let captures = header_regex.captures(&line);

            if captures.is_none() {
                let err_details = ParseErrorDetails::new_with_position(
                    ParseError::InvalidHeaderField(line),
                    (scanner.get_cursor(), None),
                );
                return Err(err_details);
            }
            let captures = captures.unwrap();
            match (captures.get(1), captures.get(2)) {
                (Some(key_match), Some(value_match)) => {
                    //@TODO: validate header fields
                    headers.push(model::Header {
                        key: key_match.as_str().to_string(),
                        value: value_match.as_str().to_string(),
                    })
                }
                _ => {
                    let err_details = ParseErrorDetails::new_with_position(
                        ParseError::InvalidHeaderField(line),
                        (scanner.get_cursor(), None),
                    );
                    return Err(err_details);
                }
            }
        }
    }

    /// Parse the body of an http request. Can either be multipart or contain some kind of data.
    /// The Jetbrains client trims the data so trailing newlines or whitespace is also ignored when
    /// parsing here
    fn parse_body(
        scanner: &mut Scanner,
        headers: &[Header],
    ) -> Result<RequestBody, (RequestBody, Vec<ParseErrorDetails>)> {
        let mut parse_errs: Vec<ParseErrorDetails> = Vec::new();
        let content_type = headers
            .iter()
            .find(|header| {
                header.key == "Content-Type" //&& header.value.starts_with("multipart/form-data")
            })
            .map(|header| header.value.as_str());

        let body = match content_type {
            Some(content_type) if content_type.starts_with("multipart/form-data") => {
                Parser::parse_content_type_multipart_form_data(
                    scanner,
                    content_type,
                    &mut parse_errs,
                )
                .unwrap_or(RequestBody::None)
            }
            Some("application/x-www-form-urlencoded") => Parser::parse_body_urlencoded(scanner),
            _ => {
                let body = Parser::parse_raw_body(scanner);
                // if we have a content-type then we just have an empty body instead of none
                if content_type.is_some() && matches!(body, RequestBody::None) {
                    RequestBody::Raw {
                        data: DataSource::Raw(String::new()),
                    }
                } else {
                    body
                }
            }
        };

        if parse_errs.is_empty() {
            Ok(body)
        } else {
            Err((body, parse_errs))
        }
    }

    fn parse_content_type_multipart_form_data(
        scanner: &mut Scanner,
        content_type: &str,
        parse_errs: &mut Vec<ParseErrorDetails>,
    ) -> Option<RequestBody> {
        let boundary_regex =
            regex::Regex::from_str("multipart/form-data\\s*(;\\s*boundary\\s*=\\s*(.+))?").unwrap();
        let captures = boundary_regex.captures(content_type);

        let mut boundary = DEFAULT_MULTIPART_BOUNDARY.to_string();

        if let Some(captures) = captures {
            let boundary_match = captures.get(2);

            // either with or without quotes
            if boundary_match.is_none() {
                parse_errs.push(ParseErrorDetails::new_with_position(
                    ParseError::MissingMultipartHeaderBoundaryDefinition(
                        DEFAULT_MULTIPART_BOUNDARY.to_string(),
                    ),
                    (scanner.get_cursor(), None),
                ));
            }
            boundary = boundary_match
                .map(|o| o.as_str())
                .unwrap_or(DEFAULT_MULTIPART_BOUNDARY)
                .to_string();
            if boundary.starts_with('"') && boundary.ends_with('"') {
                boundary = boundary[1..(boundary.len() - 1)].to_string();
            }
        } else {
            parse_errs.push(ParseErrorDetails::new_with_position(
                ParseError::MissingMultipartHeaderBoundaryDefinition(
                    DEFAULT_MULTIPART_BOUNDARY.to_string(),
                ),
                (scanner.get_cursor(), None),
            ));
        }
        if let Err(boundary_err) = Parser::is_multipart_boundary_valid(&boundary) {
            parse_errs.push(boundary_err);
        }
        match Parser::parse_multipart_body(scanner, &boundary, parse_errs) {
            Ok(multipart_body) => Some(multipart_body),
            Err(err) => {
                parse_errs.push(err);
                None
            }
        }
    }

    fn parse_body_urlencoded(scanner: &mut Scanner) -> RequestBody {
        let mut url_encoded_params: Vec<UrlEncodedParam> = Vec::new();
        if let Some(line) = scanner.peek_line() {
            let line = line.trim();
            if line.starts_with(REQUEST_SEPARATOR) {
                return RequestBody::UrlEncoded { url_encoded_params };
            }
            scanner.skip_to_next_line();
            url_encoded_params = line
                .split('&')
                .map(|key_val| {
                    let mut split = key_val.split('=');
                    let key = split.next();
                    let value = split.next();
                    UrlEncodedParam::new(key.unwrap_or_default(), value.unwrap_or_default())
                })
                .collect::<Vec<UrlEncodedParam>>();
        }

        RequestBody::UrlEncoded { url_encoded_params }
    }

    fn parse_raw_body(scanner: &mut Scanner) -> RequestBody {
        if scanner.is_done() {
            return RequestBody::None;
        }

        let start_pos = scanner.get_pos();
        loop {
            let peek_line = scanner.peek_line();
            if peek_line.is_none() {
                break;
            }
            let peek_line = peek_line.unwrap();
            // new request starts
            if peek_line.starts_with(REQUEST_SEPARATOR) {
                break;
            }

            // response handler
            if peek_line.starts_with('>') {
                // if previous line is empty then do not parse it as body before response
                // handler, when serializing we put an additional new line for clarity that
                // should not be part of the body
                if scanner
                    .get_prev_line()
                    .map_or(false, |l| l.trim().is_empty())
                {
                    scanner.step_to_previous_line_start();
                }
                break;
            }

            // output handler / redirect also ends body
            if peek_line.starts_with(">>") {
                // if previous line is empty then do not parse it as body before redirect
                // when serializing we add an additional newline before the redirect for
                // clarity which should not be part of the body
                if scanner
                    .get_prev_line()
                    .map_or(false, |l| l.trim().is_empty())
                {
                    scanner.step_to_previous_line_start();
                }
                break;
            }
            scanner.skip_to_next_line();
        }
        let mut end_pos = scanner.get_pos();
        if start_pos > end_pos {
            end_pos = start_pos.clone();
        }
        let body_str = scanner.get_from_to(start_pos, end_pos);
        if body_str.trim().starts_with('<') {
            let path = body_str.split('<').nth(1).unwrap().trim();
            RequestBody::Raw {
                data: DataSource::FromFilepath(path.to_string()),
            }
        } else if !body_str.is_empty() {
            // We trim trailing newlines, jetbrains client does the same
            // However, this means a text body cannot contain trailing newlines @TODO
            RequestBody::Raw {
                data: DataSource::Raw(body_str.trim_end_matches('\n').to_string()),
            }
        } else {
            RequestBody::None
        }
    }

    /// Parse a multipart http body
    fn parse_multipart_body(
        scanner: &mut Scanner,
        boundary: &str,
        parse_errs: &mut Vec<ParseErrorDetails>,
    ) -> Result<RequestBody, ParseErrorDetails> {
        scanner.skip_empty_lines();

        let mut parts: Vec<Multipart> = Vec::new();

        let mut errors: Vec<ParseErrorDetails> = Vec::new();
        loop {
            let multipart = Parser::parse_multipart_part(scanner, boundary, parse_errs);
            if let Err(err) = multipart {
                errors.push(err);
                break;
            }
            let multipart = multipart.unwrap();
            parts.push(multipart);
            if scanner.is_done() {
                break;
            }

            let end_boundary = format!("--{}--", boundary);
            // end of multipart
            let end_boundary = regex::escape(&end_boundary);
            if scanner.match_str_forward(&end_boundary) {
                break;
            }

            let next_boundary = format!("--{}", boundary);
            if !scanner.match_str_forward(&next_boundary) {
                let err_details = ParseErrorDetails::new_with_position(
                    ParseError::MissingMultipartBoundary {
                        next_boundary,
                        end_boundary,
                    },
                    (scanner.get_cursor(), None),
                );
                return Err(err_details);
            }
        }
        Ok(RequestBody::Multipart {
            boundary: boundary.to_string(),
            parts,
        })
    }

    /// Parse a single block of a multipart body
    fn parse_multipart_part(
        scanner: &mut Scanner,
        boundary: &str,
        parse_errs: &mut Vec<ParseErrorDetails>,
    ) -> Result<model::Multipart, ParseErrorDetails> {
        let boundary_line = format!("--{}", boundary);
        let multipart_end_line = format!("--{}--", boundary);

        let escaped_boundary = regex::escape(&boundary_line);
        let first_boundary = scanner.match_regex_forward(&escaped_boundary);
        if first_boundary.is_err() {
            return Err(ParseErrorDetails::new_with_position(
                ParseError::MissingMultipartStartingBoundary,
                (scanner.get_cursor(), None),
            ));
        }

        scanner.skip_to_next_line(); // @TODO: nothing else should be here

        let start_pos = scanner.get_pos();

        let part_headers = Parser::parse_headers(scanner).map_err(|err| {
            ParseErrorDetails::new_with_position(
                ParseError::InvalidSingleMultipartHeaders {
                    header_parse_err: Box::new(err.error.clone()),
                    error_msg: err.error.to_string(),
                },
                (scanner.get_cursor(), None),
            )
        })?;
        let end_pos = scanner.get_pos();

        let (field, part_headers) = match &part_headers[..] {
            [] => {
                return Err(ParseErrorDetails::new_with_position(
                    ParseError::MissingSingleMultipartContentDispositionHeader,
                    (start_pos.cursor, Some(end_pos.cursor)),
                ));
            }
            [disposition_part, part_headers @ ..] => {
                if disposition_part.key != "Content-Disposition" {
                    return Err(ParseErrorDetails::new_with_position(
                        ParseError::WrongMultipartContentDispositionHeader(
                            disposition_part.key.clone(),
                        ),
                        (start_pos.cursor, Some(end_pos.cursor)),
                    ));
                }
                let parts: Vec<&str> = disposition_part.value.split(';').collect();
                let mut parts_iter = parts.iter();
                let disposition_type = parts_iter.next().unwrap().trim();
                if disposition_type != "form-data" {
                    // only form-data is valid in http context, other disposition types may exist
                    // for other applications (email mime types...)
                    return Err(ParseErrorDetails::new_with_position(
                        ParseError::InvalidMultipartContentDispositionFormData(
                            disposition_type.to_string(),
                        ),
                        (start_pos.cursor, Some(end_pos.cursor)),
                    ));
                }
                let mut disposition_field = DispositionField::new_with_filename("", None::<String>);
                for current in parts_iter {
                    match current.split('=').map(|p| p.trim()).collect::<Vec<&str>>()[..] {
                        [key, mut value] => {
                            if value.starts_with('"') && value.ends_with('"') {
                                value = &value[1..(value.len() - 1)];
                            }
                            if key == "filename" {
                                disposition_field.filename = Some(value.to_string());
                            } else if key == "filename*" {
                                disposition_field.filename_star = Some(value.to_string());
                            } else if key == "name" {
                                disposition_field.name = value.to_string();
                            }
                        }
                        _ => {
                            return Err(ParseErrorDetails::from(
                                ParseError::MalformedContentDispositionEntries(current.to_string()),
                            ))
                        }
                    }
                }
                (disposition_field, part_headers)
            }
        };

        if field.name.is_empty() {
            let msg = format!(
                "[{}]",
                part_headers
                    .iter()
                    .map(|header| header.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            );
            parse_errs.push(ParseErrorDetails::new_with_position(
                ParseError::SingleMultipartNameMissing(msg),
                (start_pos.cursor, Some(end_pos.cursor)),
            ));
        }

        if !scanner.match_str_forward("\n") {
            return Err(ParseErrorDetails::new_with_position(
                ParseError::SingleMultipartMissingEmptyLine,
                (scanner.get_cursor(), None),
            ));
        }

        let peek_line = scanner.peek_line();

        if peek_line.is_none() {
            return Err(ParseErrorDetails {
                error: ParseError::MultipartShouldBeEndedWithBoundary(multipart_end_line),
                ..Default::default()
            });
        }

        let peek_line = peek_line.unwrap();

        // < means content of multipart is read from file
        // should only have one line to parse
        // @TODO only read in file depending on the content type -> how is this not ambigous?
        // @TODO can we have multiple files added here?
        if peek_line.starts_with('<') {
            let mut line = scanner.get_line_and_advance().unwrap();
            line = line.trim().to_string();

            let file_path = &line[1..].trim();
            // @TODO is name expected?
            Ok(Multipart {
                disposition: field,
                headers: part_headers.to_vec(),
                data: DataSource::FromFilepath(file_path.to_string()), // @TODO: when to read in data from file?
            })
        } else {
            let mut text = String::new();

            loop {
                let peek_line = scanner.peek_line();
                if peek_line.is_none() {
                    return Err(ParseErrorDetails {
                        error: ParseError::MultipartShouldBeEndedWithBoundary(multipart_end_line),
                        ..Default::default()
                    });
                };
                let peek_line = peek_line.unwrap();
                if peek_line == boundary_line || peek_line == multipart_end_line {
                    return Ok(Multipart {
                        disposition: field,
                        headers: part_headers.to_owned(),
                        data: DataSource::Raw(text),
                    });
                }
                let next = scanner.get_line_and_advance().unwrap();
                text += &next;
                // only add a new line if more text will appear
                if !scanner
                    .peek_line()
                    .map_or(false, |pl| pl.starts_with(&boundary_line))
                {
                    text += "\n";
                }
            }
        }
    }

    /// Checks whether a multipart boundary is valid or not according to: https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1
    fn is_multipart_boundary_valid(boundary: &str) -> Result<(), ParseErrorDetails> {
        let boundary_len = boundary.len();
        if !(1..=70).contains(&boundary_len) {
            return Err(ParseErrorDetails {
                error: ParseError::InvalidMultipartBoundaryLength,
                ..Default::default()
            });
        }

        let bytes = boundary.as_bytes();
        for byte in bytes {
            match byte {
                b'0'..=b'9'
                | b'a'..=b'z'
                | b'A'..=b'Z'
                | b'\''
                | b'('
                | b')'
                | b'.'
                | b','
                | b'-'
                | b'_'
                | b'+'
                | b'/'
                | b':'
                | b'?'
                | b'=' => continue,
                invalid_byte => {
                    return Err(ParseErrorDetails {
                        error: ParseError::InvalidMultipartBoundaryCharacter(
                            String::from_utf8(vec![invalid_byte.to_owned()]).unwrap(),
                        ),
                        ..Default::default()
                    });
                }
            }
        }
        Ok(())
    }

    /// Parse a response handler. The http client can also pass the response data to a javascript block or to javascript code
    /// within a file if given as a path. This function parses either a path or the script as
    /// string similar to the `parse_pre_request_script` function.
    fn parse_response_handler(
        scanner: &mut Scanner,
    ) -> Result<Option<model::ResponseHandler>, ParseErrorDetails> {
        scanner.skip_empty_lines();
        scanner.skip_ws();
        let next_two = scanner.peek_n(2);
        if next_two.is_none() {
            return Ok(None);
        }
        let next_two = next_two.unwrap();
        if next_two[0] != '>' || next_two[1] == '>' {
            return Ok(None);
        }

        if !scanner.take(&'>') {
            return Ok(None);
        }
        scanner.skip_ws();
        scanner.skip_empty_lines();
        let start_pos = scanner.get_pos();
        if scanner.match_str_forward("{%") {
            let mut lines: Vec<String> = Vec::new();
            let mut found = false;
            loop {
                if let Ok(Some(matches)) = scanner.match_regex_forward("(.*)%}") {
                    for m in matches {
                        found = true;
                        lines.push(m.to_string());
                    }
                    if found {
                        break;
                    }
                } else {
                    let line = scanner.get_line_and_advance();
                    if line.is_none() {
                        break;
                    }
                    lines.push(line.unwrap());
                }
            }
            if !found {
                return Err(ParseErrorDetails::new_with_position(
                    ParseError::MissingResponseHandlerClose,
                    (start_pos.cursor, Some(scanner.get_cursor())),
                ));
            }

            scanner.skip_to_next_line();

            Ok(Some(ResponseHandler::Script(lines.join("\n"))))
        } else {
            let path = scanner.get_line_and_advance();
            if path.is_none() || path.as_ref().unwrap().is_empty() {
                return Err(ParseErrorDetails::new_with_position(
                    ParseError::MissingResponseHandlerClose,
                    (scanner.get_cursor(), None::<usize>),
                ));
            }

            return Ok(Some(ResponseHandler::FromFilepath(
                path.unwrap().trim().to_string(),
            )));
        }
    }

    /// Parse a redirect line. A redirect can specify where the response of an http request should
    /// be saved. A redirect line either has the form `>> <some/path>` or `>>! <some/path>`
    fn parse_redirect(scanner: &mut Scanner) -> Result<Option<SaveResponse>, ParseErrorDetails> {
        scanner.skip_empty_lines();
        let start_pos = scanner.get_pos();
        if !scanner.match_str_forward(">>") {
            return Ok(None);
        }

        let mut rewrite = false;
        if scanner.take(&'!') {
            rewrite = true;
        }

        let path = scanner.get_line_and_advance();

        if path.is_none() {
            return Err(ParseErrorDetails::new_with_position(
                ParseError::MissingResponseOutputPath,
                (start_pos.cursor, Some(scanner.get_cursor())),
            ));
        }

        let path = path.unwrap().trim().to_string();

        if rewrite {
            Ok(Some(SaveResponse::RewriteFile(std::path::PathBuf::from(
                path,
            ))))
        } else {
            Ok(Some(SaveResponse::NewFileIfExists(
                std::path::PathBuf::from(path),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{Comment, DispositionField, HttpMethod, Request, RequestLine},
        parser::model::{Header, HttpVersion},
    };

    use super::*;

    #[test]
    pub fn name_triple_tag() {
        let str = "
### test name

https://httpbin.org
";
        let parsed = Parser::parse(str, false);

        let expected = vec![model::Request {
            name: Some(String::from("test name")),
            comments: Vec::new(),
            request_line: model::RequestLine {
                method: WithDefault::default(),
                target: RequestTarget::from("https://httpbin.org"),
                http_version: WithDefault::default(),
            },
            headers: Vec::new(),
            body: model::RequestBody::None,
            settings: RequestSettings::default(),
            pre_request_script: None,
            response_handler: None,
            save_response: None,
        }];

        assert!(parsed.errs.is_empty());
        assert_eq!(parsed.requests, expected);
    }

    #[test]
    pub fn name_with_at() {
        let str = "
# @name=test name

https://httpbin.org
";
        let parsed = Parser::parse(str, false);

        let expected = vec![model::Request {
            name: Some("test name".to_string()),
            comments: Vec::new(),
            request_line: model::RequestLine {
                method: WithDefault::default(),
                target: RequestTarget::from("https://httpbin.org"),
                http_version: WithDefault::default(),
            },
            headers: Vec::new(),
            body: model::RequestBody::None,
            settings: RequestSettings::default(),
            pre_request_script: None,
            response_handler: None,
            save_response: None,
        }];

        assert!(parsed.errs.is_empty());
        assert_eq!(parsed.requests, expected)
    }

    #[test]
    pub fn comment_and_name_tag() {
        let str = "
### Just a comment
## invalid comment but still parsed
# @name=actual request name

GET https://test.com
";
        // if there is a ### comment and a @name section use the @name section as name
        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert!(requests.len() == 1);
        let request = requests.remove(0);
        assert!(errs.len() == 0);
        assert_eq!(request.name, Some("actual request name".to_string()));
        assert_eq!(request.comments.len(), 2);
        assert_eq!(
            request.comments,
            vec![
                Comment {
                    value: "Just a comment".to_string(),
                    kind: CommentKind::RequestSeparator
                },
                Comment {
                    value: "# invalid comment but still parsed".to_string(),
                    kind: CommentKind::SingleTag
                }
            ]
        );
    }

    #[test]
    pub fn custom_method() {
        let str = "
# @name=test name

CUSTOMVERB https://httpbin.org
";
        let parsed = Parser::parse(str, false);

        let expected = vec![model::Request {
            name: Some(String::from("test name")),
            comments: Vec::new(),
            request_line: model::RequestLine {
                method: WithDefault::Some(model::HttpMethod::CUSTOM("CUSTOMVERB".to_string())),
                target: RequestTarget::from("https://httpbin.org"),
                http_version: WithDefault::default(),
            },
            headers: Vec::new(),
            body: model::RequestBody::None,
            settings: RequestSettings::default(),
            pre_request_script: None,
            response_handler: None,
            save_response: None,
        }];

        assert!(parsed.errs.is_empty());
        assert_eq!(parsed.requests, expected);
    }

    #[test]
    pub fn no_body_post() {
        let str = "
# @name=test name

POST https://httpbin.org
";
        let parsed = Parser::parse(str, false);

        let expected = vec![model::Request {
            name: Some("test name".to_string()),
            comments: Vec::new(),
            request_line: model::RequestLine {
                method: WithDefault::Some(HttpMethod::POST),
                target: RequestTarget::from("https://httpbin.org"),
                http_version: WithDefault::default(),
            },
            headers: Vec::new(),
            body: model::RequestBody::None,
            settings: RequestSettings::default(),
            pre_request_script: None,
            response_handler: None,
            save_response: None,
        }];

        assert!(parsed.errs.is_empty());
        assert_eq!(parsed.requests, expected);
    }

    #[test]
    pub fn name_with_whitespace() {
        let str = "
# @name  =  test name    

POST https://httpbin.org
";
        let parsed = Parser::parse(str, false);

        let expected = vec![model::Request {
            name: Some(String::from("test name")),
            comments: Vec::new(),
            request_line: model::RequestLine {
                method: WithDefault::Some(HttpMethod::POST),
                target: RequestTarget::from("https://httpbin.org"),
                http_version: WithDefault::default(),
            },
            headers: Vec::new(),
            body: model::RequestBody::None,
            settings: RequestSettings::default(),
            pre_request_script: None,
            response_handler: None,
            save_response: None,
        }];

        // whitespace before or after name should be removed
        assert_eq!(parsed.requests[0].name, Some("test name".to_string()));
        assert!(parsed.errs.is_empty());
        assert_eq!(parsed.requests, expected);
    }

    #[test]
    pub fn multiple_comments() {
        let str = "
### Comment one
### Comment line two    
// This comment type is also allowed      
# @name  =  test name    

POST https://httpbin.org
";
        let parsed = Parser::parse(str, false);

        assert!(parsed.errs.is_empty());
        assert_eq!(
            parsed.requests[0].get_comment_text(),
            Some(
                "Comment one\nComment line two    \nThis comment type is also allowed      "
                    .to_string()
            ),
            "parsed: {:?}, {:?}",
            parsed.requests,
            parsed.errs
        );
    }

    #[test]
    pub fn parse_meta_name_line() {
        let str = "@name  =  actual request name";
        let mut scanner = Scanner::new(str);
        let name = Parser::parse_meta_name(&mut scanner)
            .expect("can parse name line without error")
            .expect("parse returns something");
        assert_eq!(name, "actual request name".to_string());
    }

    #[test]
    pub fn request_target_asterisk() {
        let FileParseResult { mut requests, errs } = Parser::parse("*", false);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(request.request_line.target, RequestTarget::Asterisk);
        assert_eq!(errs, vec![]);

        // @TODO: is asterisk form only for OPTIONS request?
        let FileParseResult { mut requests, errs } = Parser::parse("GET *", false);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(request.request_line.target, RequestTarget::Asterisk);
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
        assert_eq!(request.request_line.http_version, WithDefault::default());
        assert_eq!(errs, vec![]);

        let FileParseResult { mut requests, errs } =
            Parser::parse("CUSTOMMETHOD * HTTP/1.1", false);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(request.request_line.target, RequestTarget::Asterisk);
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::CUSTOM(String::from("CUSTOMMETHOD")))
        );
        assert_eq!(
            request.request_line.http_version,
            WithDefault::Some(model::HttpVersion { major: 1, minor: 1 })
        );
        assert_eq!(errs, vec![]);
    }

    #[test]
    pub fn request_target_absolute() {
        let FileParseResult { mut requests, errs } =
            Parser::parse("https://test.com/api/v1/user?show_all=true&limit=10", false);

        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        // only with relative url
        let expected_target = RequestTarget::Absolute {
            uri: "https://test.com/api/v1/user?show_all=true&limit=10".to_string(),
        };
        assert_eq!(request.request_line.target, expected_target);

        match request.request_line.target {
            RequestTarget::Absolute { ref uri } => {
                assert_eq!(uri, "https://test.com/api/v1/user?show_all=true&limit=10");
            }
            _ => panic!("not expected target found"),
        }

        assert!(request.request_line.target.has_scheme());
        assert_eq!(errs, vec![]);

        // method and URL
        let FileParseResult { requests, errs } = Parser::parse(
            "GET https://test.com/api/v1/user?show_all=true&limit=10",
            false,
        );
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.request_line.target, expected_target);
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
        assert_eq!(request.request_line.http_version, WithDefault::default());
        assert_eq!(errs, vec![]);

        // method and URL and http version
        let FileParseResult { mut requests, errs } = Parser::parse(
            "GET https://test.com/api/v1/user?show_all=true&limit=10    HTTP/1.1",
            false,
        );
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(request.request_line.target, expected_target);
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
        assert_eq!(
            request.request_line.http_version,
            WithDefault::Some(model::HttpVersion { major: 1, minor: 1 })
        );
        assert_eq!(errs, vec![]);
    }

    #[test]
    pub fn request_target_no_scheme_with_host_no_path() {
        let FileParseResult { mut requests, errs } = Parser::parse("test.com", false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        match request.request_line.target {
            RequestTarget::Absolute { ref uri } => {
                assert_eq!(uri, "test.com");
            }
            kind => panic!("!request target is not absolute kind, it is: {:?}", kind),
        }
    }

    #[test]
    pub fn request_target_no_scheme_with_host_and_path() {
        let FileParseResult { mut requests, errs } = Parser::parse("test.com/api/v1/test", false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        match request.request_line.target {
            RequestTarget::Absolute { ref uri } => {
                // @TODO: with uri parser we cannot have
                // authority and path without a scheme, add http as default in this case if no
                // scheme is present

                assert_eq!(uri, "test.com/api/v1/test");
            }
            kind => panic!("!request target is not absolute kind, it is: {:?}", kind),
        }
    }

    #[test]
    pub fn request_target_relative() {
        let FileParseResult { mut requests, errs } =
            Parser::parse("/api/v1/user?show_all=true&limit=10", false);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        // only with relative url
        let expected_target = RequestTarget::RelativeOrigin {
            uri: "/api/v1/user?show_all=true&limit=10".to_string(),
        };
        assert_eq!(request.request_line.target, expected_target);

        match request.request_line.target {
            RequestTarget::RelativeOrigin { ref uri } => {
                assert_eq!(uri, "/api/v1/user?show_all=true&limit=10");
            }
            _ => panic!("not expected target found"),
        }

        assert!(!request.request_line.target.has_scheme());
        assert_eq!(errs, vec![]);

        // method and URL
        let FileParseResult { mut requests, errs } =
            Parser::parse("GET /api/v1/user?show_all=true&limit=10", false);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(request.request_line.target, expected_target);
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
        assert_eq!(request.request_line.http_version, WithDefault::default());
        assert_eq!(errs, vec![]);

        // method and URL and http version
        let FileParseResult { mut requests, errs } =
            Parser::parse("GET /api/v1/user?show_all=true&limit=10    HTTP/1.1", false);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(request.request_line.target, expected_target);
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
        assert_eq!(
            request.request_line.http_version,
            WithDefault::Some(model::HttpVersion { major: 1, minor: 1 })
        );
        assert_eq!(errs, vec![]);
    }

    #[test]
    pub fn validate_http_version() {
        let version = model::HttpVersion::from_str("HTTP/1.1").expect("Version 1.1 to be valid");
        assert_eq!(version, model::HttpVersion { major: 1, minor: 1 });

        let version = model::HttpVersion::from_str("HTTP/1.2").expect("Version 1.2 to be valid");
        assert_eq!(version, model::HttpVersion { major: 1, minor: 2 });

        let version = model::HttpVersion::from_str("HTTP/2.0").expect("Version 2.0 to be valid");
        assert_eq!(version, model::HttpVersion { major: 2, minor: 0 });

        let version = model::HttpVersion::from_str("HTTP/2.1").expect("Version 2.1 to be valid");
        assert_eq!(version, model::HttpVersion { major: 2, minor: 1 });

        assert!(model::HttpVersion::from_str("invalid").is_err());
    }

    #[test]
    pub fn request_target_multiline() {
        let str = r#####"
GET https://test.com:8080
    /get
    /html
    ?id=123
    &value=test

        "#####;
        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(
            request.request_line.target,
            RequestTarget::Absolute {
                uri: "https://test.com:8080/get/html?id=123&value=test".to_owned()
            }
        );
        assert_eq!(request.request_line.http_version, WithDefault::default());
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
    }

    #[test]
    pub fn request_target_multiline_no_method() {
        let str = r#####"
https://test.com:8080
    /get
    /html
    ?id=123
    &value=test

        "#####;
        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(
            request.request_line.target,
            RequestTarget::Absolute {
                uri: "https://test.com:8080/get/html?id=123&value=test".to_owned()
            }
        );
        assert_eq!(request.request_line.http_version, WithDefault::default());
        assert_eq!(request.request_line.method, WithDefault::default());
    }

    #[test]
    pub fn request_target_multiline_with_version() {
        let str = r#####"
GET https://test.com:8080
    /get
    /html
    ?id=123
    &value=test HTTP/2.1

        "#####;
        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);
        assert_eq!(
            request.request_line.target,
            RequestTarget::Absolute {
                uri: "https://test.com:8080/get/html?id=123&value=test".to_owned()
            }
        );
        assert_eq!(
            request.request_line.http_version,
            WithDefault::Some(HttpVersion { major: 2, minor: 1 })
        );
        assert_eq!(
            request.request_line.method,
            WithDefault::Some(HttpMethod::GET)
        );
    }

    #[test]
    pub fn parse_simple_headers() {
        let str = "Key1: Value1
Key2: Value2
Key3: Value3
";
        let mut scanner = Scanner::new(str);
        let parsed = Parser::parse_headers(&mut scanner);

        let parsed = parsed.expect("No error for simple headers");

        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], Header::new("Key1", "Value1"));
        assert_eq!(parsed[1], Header::new("Key2", "Value2"));
        assert_eq!(parsed[2], Header::new("Key3", "Value3"));
    }

    #[test]
    pub fn parse_headers_with_colon() {
        let str = r###"Host: localhost:8080
Custom: ::::::

        "###;
        let mut scanner = Scanner::new(str);
        let parsed = Parser::parse_headers(&mut scanner).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], Header::new("Host", "localhost:8080"));
        assert_eq!(parsed[1], Header::new("Custom", "::::::"));
    }

    #[test]
    pub fn parse_with_multipart_body_file() {
        let str = r####"
# With Multipart Body
POST https://test.com/multipart
Content-Type: multipart/form-data; boundary="--test_boundary"

----test_boundary
Content-Disposition: form-data; name="part1_name"

< path/to/file
----test_boundary--
"####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new(
                "Content-Type",
                "multipart/form-data; boundary=\"--test_boundary\""
            )]
        );

        assert_eq!(
            request.body,
            model::RequestBody::Multipart {
                boundary: "--test_boundary".to_string(),
                parts: vec![Multipart {
                    disposition: DispositionField::new_with_filename("part1_name", None::<String>),
                    data: DataSource::FromFilepath("path/to/file".to_string()),
                    headers: vec![]
                }]
            }
        )
    }

    #[test]
    pub fn parse_with_multipart_body_text() {
        let str = r####"
# With Multipart Body
POST https://test.com/multipart
Content-Type: multipart/form-data; boundary="--test.?)()test"

----test.?)()test
Content-Disposition: form-data; name="text"

some text

----test.?)()test
Content-Disposition: form-data; name="text"

more content


----test.?)()test--
"####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new(
                "Content-Type",
                "multipart/form-data; boundary=\"--test.?)()test\""
            )]
        );

        assert_eq!(
            request.body,
            model::RequestBody::Multipart {
                boundary: "--test.?)()test".to_string(),
                parts: vec![
                    Multipart {
                        disposition: DispositionField::new("text"),
                        headers: vec![],
                        data: DataSource::Raw("some text\n".to_string()),
                    },
                    Multipart {
                        disposition: DispositionField::new("text"),
                        headers: vec![],
                        data: DataSource::Raw("more content\n\n".to_string()),
                    }
                ]
            }
        )
    }

    #[test]
    pub fn parse_multipart_with_content_types() {
        let str = r#####"
### Send a form with the text and file fields
POST https://httpbin.org/post
Content-Type: multipart/form-data; boundary=WebAppBoundary

--WebAppBoundary
Content-Disposition: form-data; name="element-name"
Content-Type: text/plain

Name
--WebAppBoundary
Content-Disposition: form-data; name="data"; filename="data.json"
Content-Type: application/json

< ./request-form-data.json
--WebAppBoundary--
        "#####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);

        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new(
                "Content-Type",
                "multipart/form-data; boundary=WebAppBoundary"
            )]
        );

        assert_eq!(
            request.body,
            model::RequestBody::Multipart {
                boundary: "WebAppBoundary".to_string(),
                parts: vec![
                    Multipart {
                        data: DataSource::Raw("Name".to_string()),
                        disposition: DispositionField::new("element-name"),
                        headers: vec![Header {
                            key: "Content-Type".to_string(),
                            value: "text/plain".to_string()
                        }]
                    },
                    Multipart {
                        data: DataSource::FromFilepath("./request-form-data.json".to_string()),
                        disposition: DispositionField::new_with_filename("data", Some("data.json")),
                        headers: vec![Header {
                            key: "Content-Type".to_string(),
                            value: "application/json".to_string()
                        }]
                    }
                ]
            }
        )
    }

    #[test]
    pub fn parse_multipart_binary() {
        let str = r#####"
POST /upload HTTP/1.1
Host: localhost:8080
Content-Type: multipart/form-data; boundary=/////////////////////////////
Content-Length: 676

--/////////////////////////////
Content-Disposition: form-data; name="file"; filename="binaryfile.tar.gz"
Content-Type: application/x-gzip
Content-Transfer-Encoding: base64

H4sIAGiNIU8AA+3R0W6CMBQGYK59iobLZantRDG73osUOGqnFNJWM2N897UghG1ZdmWWLf93U/jP4bRAq8q92hJ/dY1J7kQEqyyLq8yXYrp2ltkqkTKXYiEykYc++ZTLVcLEvQ40dXReWcYSV1pdnL/v+6n+R11mjKVG1ZQ+s3TT2FpXqjhQ+hjzE1mnGxNLkgu+7tOKWjIVmVKTC6XL9ZaeXj4VQhwKWzL+cI4zwgQuuhkh3mhTad/Hkssh3im3027X54JnQ360R/M19OT8kC7SEN7Ooi2VvrEfznHQRWzl83gxttZKmzGehzPRW/+W8X+3fvL8sFet9sS6m3EIma02071MU3Uf9KHrmV1/+y8DAAAAAAAAAAAAAAAAAAAAAMB/9A6txIuJACgAAA==
--/////////////////////////////--
        "#####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![
                Header::new("Host", "localhost:8080"),
                Header::new(
                    "Content-Type",
                    r#"multipart/form-data; boundary=/////////////////////////////"#
                ),
                Header::new("Content-Length", "676")
            ]
        );

        // @TODO check content
        assert_eq!(
            request.body,
            model::RequestBody::Multipart {
                boundary: r#"/////////////////////////////"#.to_string(),
                parts: vec![model::Multipart {
                    disposition: DispositionField::new_with_filename("file", Some("binaryfile.tar.gz")),
                    headers: vec![
                        Header {
                            key: "Content-Type".to_string(),
                            value: "application/x-gzip".to_string()
                        },
                        Header {
                            key: "Content-Transfer-Encoding".to_string(),
                            value: "base64".to_string()
                        }
                    ],
                    data: DataSource::Raw("H4sIAGiNIU8AA+3R0W6CMBQGYK59iobLZantRDG73osUOGqnFNJWM2N897UghG1ZdmWWLf93U/jP4bRAq8q92hJ/dY1J7kQEqyyLq8yXYrp2ltkqkTKXYiEykYc++ZTLVcLEvQ40dXReWcYSV1pdnL/v+6n+R11mjKVG1ZQ+s3TT2FpXqjhQ+hjzE1mnGxNLkgu+7tOKWjIVmVKTC6XL9ZaeXj4VQhwKWzL+cI4zwgQuuhkh3mhTad/Hkssh3im3027X54JnQ360R/M19OT8kC7SEN7Ooi2VvrEfznHQRWzl83gxttZKmzGehzPRW/+W8X+3fvL8sFet9sS6m3EIma02071MU3Uf9KHrmV1/+y8DAAAAAAAAAAAAAAAAAAAAAMB/9A6txIuJACgAAA==".to_string())
                }]
            }
        )
    }

    #[test]
    pub fn parse_json_body() {
        let str = r#####"
GET http://localhost/api/json/get?id=12345
Authorization: Basic dev-user dev-password
Content-Type: application/json

{
    "key": "my-dev-value"
}"#####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);

        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![
                Header::new("Authorization", r#"Basic dev-user dev-password"#),
                Header::new("Content-Type", "application/json")
            ]
        );

        assert_eq!(
            request.body,
            model::RequestBody::Raw {
                data: DataSource::Raw(
                    r#"{
    "key": "my-dev-value"
}"#
                    .to_string()
                )
            }
        )
    }

    #[test]
    pub fn parse_json_body_fileinput() {
        let str = r#####"
POST http://example.com/api/add
Content-Type: application/json

< ./input.json

        "#####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);

        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new("Content-Type", "application/json")]
        );

        // @TODO check content
        assert_eq!(
            request.body,
            model::RequestBody::Raw {
                data: DataSource::FromFilepath("./input.json".to_string())
            }
        )
    }

    #[test]
    pub fn parse_url_form_encoded_end_of_file() {
        let str = r####"# @name=Create Checkout Session
POST {{base_url}}/create_checkout_session?a=aa
Content-Type: application/x-www-form-urlencoded

abc=def&ghi=jkl"####;
        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new(
                "Content-Type",
                "application/x-www-form-urlencoded"
            )]
        );

        assert_eq!(
            request.body,
            RequestBody::UrlEncoded {
                url_encoded_params: vec![
                    UrlEncodedParam::new("abc", "def"),
                    UrlEncodedParam::new("ghi", "jkl"),
                ]
            }
        )
    }

    #[test]
    pub fn parse_url_form_encoded() {
        let str = r####"
POST https://test.com/formEncoded
Content-Type: application/x-www-form-urlencoded

firstKey=firstValue&secondKey=secondValue&empty=
"####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new(
                "Content-Type",
                "application/x-www-form-urlencoded"
            )]
        );

        assert_eq!(
            request.body,
            RequestBody::UrlEncoded {
                url_encoded_params: vec![
                    UrlEncodedParam::new("firstKey", "firstValue"),
                    UrlEncodedParam::new("secondKey", "secondValue"),
                    UrlEncodedParam::new("empty", ""),
                ]
            }
        )
    }

    #[test]
    pub fn parse_multiple_requests() {
        let str = r#####"
POST http://example.com/api/add
Content-Type: application/json

< ./input.json
###

GET https://example.com/first
###
GET https://example.com/second


###
        "#####;

        let FileParseResult { requests, errs } = dbg!(Parser::parse(str, false));
        println!("errs: {:?}", errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(requests.len(), 3);

        // @TODO check content
        assert_eq!(
            requests,
            vec![
                model::Request {
                    name: None,
                    comments: vec![],
                    headers: vec![Header {
                        key: "Content-Type".to_string(),
                        value: "application/json".to_string()
                    }],
                    body: model::RequestBody::Raw {
                        data: DataSource::FromFilepath("./input.json".to_string())
                    },
                    request_line: model::RequestLine {
                        http_version: WithDefault::default(),
                        method: WithDefault::Some(HttpMethod::POST),
                        target: model::RequestTarget::Absolute {
                            uri: "http://example.com/api/add".to_string()
                        }
                    },
                    settings: RequestSettings::default(),
                    pre_request_script: None,
                    response_handler: None,
                    save_response: None,
                },
                model::Request {
                    name: None,
                    comments: vec![],
                    headers: vec![],
                    body: model::RequestBody::None,
                    request_line: model::RequestLine {
                        http_version: WithDefault::default(),
                        method: WithDefault::Some(HttpMethod::GET),
                        target: model::RequestTarget::Absolute {
                            uri: "https://example.com/first".to_string()
                        }
                    },
                    settings: RequestSettings::default(),
                    pre_request_script: None,
                    response_handler: None,
                    save_response: None,
                },
                model::Request {
                    name: None,
                    comments: vec![],
                    headers: vec![],
                    body: model::RequestBody::None,
                    request_line: model::RequestLine {
                        http_version: WithDefault::default(),
                        method: WithDefault::Some(HttpMethod::GET),
                        target: model::RequestTarget::Absolute {
                            uri: "https://example.com/second".to_string()
                        }
                    },
                    settings: RequestSettings::default(),
                    pre_request_script: None,
                    response_handler: None,
                    save_response: None
                }
            ],
        );
    }

    #[test]
    pub fn parse_meta_directives() {
        let str = r#####"
### The Request
# @no-redirect
// @no-log
// @name= RequestName
# @no-cookie-jar
GET https://httpbin.org
"#####;
        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("RequestName".to_string()),
                headers: vec![],
                comments: vec![Comment {
                    value: "The Request".to_string(),
                    kind: CommentKind::RequestSeparator
                }],
                settings: RequestSettings {
                    no_redirect: Some(true),
                    no_log: Some(true),
                    no_cookie_jar: Some(true),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: None,
                response_handler: None,
                save_response: None
            }
        );
    }

    #[test]
    pub fn parse_pre_request_script_single_line() {
        let str = r#####"
### Request
< {%     request.variables.set("firstname", "John") %}
// @no-log
GET https://httpbin.org
"#####;
        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("Request".to_string()),
                headers: vec![],
                comments: vec![],
                settings: RequestSettings {
                    no_redirect: Some(false),
                    no_log: Some(true),
                    no_cookie_jar: Some(false),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: Some(model::PreRequestScript::Script(
                    r#"     request.variables.set("firstname", "John") "#.to_string()
                )),
                response_handler: None,
                save_response: None
            }
        );
    }

    #[test]
    pub fn parse_pre_request_script_multiple_lines() {
        let str = r#####"
### Request
< {%
 const signature = crypto.hmac.sha256()
        .withTextSecret(request.environment.get("secret")) // get variable from http-client.private.env.json
        .updateWithText(request.body.tryGetSubstituted())
        .digest().toHex();
    request.variables.set("signature", signature)

    const hash = crypto.sha256()
        .updateWithText(request.body.tryGetSubstituted())
        .digest().toHex();
    request.variables.set("hash", hash)
%}
// @no-log
GET https://httpbin.org
"#####;

        let pre_request_script = r#####"
 const signature = crypto.hmac.sha256()
        .withTextSecret(request.environment.get("secret")) // get variable from http-client.private.env.json
        .updateWithText(request.body.tryGetSubstituted())
        .digest().toHex();
    request.variables.set("signature", signature)

    const hash = crypto.sha256()
        .updateWithText(request.body.tryGetSubstituted())
        .digest().toHex();
    request.variables.set("hash", hash)
"#####;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("Request".to_string()),
                headers: vec![],
                comments: vec![],
                settings: RequestSettings {
                    no_redirect: Some(false),
                    no_log: Some(true),
                    no_cookie_jar: Some(false),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: Some(model::PreRequestScript::Script(
                    pre_request_script.to_string()
                )),
                response_handler: None,
                save_response: None,
            }
        );
    }

    #[test]
    pub fn parse_pre_request_script_variable_rename() {
        let str = r#####"
### Request
< {% request.variables.set("firstname", "John") %}
// @no-log
GET https://httpbin.org/{{firstname}}
"#####;
        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("Request".to_string()),
                headers: vec![],
                comments: vec![],
                settings: RequestSettings {
                    no_redirect: Some(false),
                    no_log: Some(true),
                    no_cookie_jar: Some(false),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org/John"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: Some(model::PreRequestScript::Script(
                    r#" request.variables.set("firstname", "John") "#.to_string()
                )),
                response_handler: None,
                save_response: None
            }
        );
    }

    #[test]
    pub fn parse_pre_request_script_variable_rename_multiline() {
        let str = r#####"
### Request
< {%
    request.variables.set("firstname", "John")
    request.variables.set("domain", "httpbin")
%}
// @no-log
GET https://{{domain}}.org/{{firstname}}
"#####;

        let pre_request_script = r####"
    request.variables.set("firstname", "John")
    request.variables.set("domain", "httpbin")
"####;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("Request".to_string()),
                headers: vec![],
                comments: vec![],
                settings: RequestSettings {
                    no_redirect: Some(false),
                    no_log: Some(true),
                    no_cookie_jar: Some(false),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org/John"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: Some(model::PreRequestScript::Script(
                    pre_request_script.to_string()
                )),
                response_handler: None,
                save_response: None
            }
        );
    }

    #[test]
    pub fn parse_handler_script_single_line() {
        let str = r#####"
### Request
// @no-log
GET https://httpbin.org

> {% client.global.set("my_cookie", response.headers.valuesOf("Set-Cookie")[0]); %} 
"#####;

        let response_handler_script = r#####" client.global.set("my_cookie", response.headers.valuesOf("Set-Cookie")[0]); "#####;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("Request".to_string()),
                headers: vec![],
                comments: vec![],
                settings: RequestSettings {
                    no_redirect: Some(false),
                    no_log: Some(true),
                    no_cookie_jar: Some(false),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: None,
                response_handler: Some(ResponseHandler::Script(
                    response_handler_script.to_string()
                )),
                save_response: None
            }
        );
    }
    #[test]
    pub fn parse_handler_script_multiple_lines() {
        let str = r#####"
### Request
// @no-log
GET https://httpbin.org

> {%
    client.global.set("my_cookie", response.headers.valuesOf("Set-Cookie")[0]);
    client.global.set("my_cookie_2", response.headers.valuesOf("Set-Cookie")[0]);
%} 
"#####;

        let response_handler_script = r#####"
    client.global.set("my_cookie", response.headers.valuesOf("Set-Cookie")[0]);
    client.global.set("my_cookie_2", response.headers.valuesOf("Set-Cookie")[0]);
"#####;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("Request".to_string()),
                headers: vec![],
                comments: vec![],
                settings: RequestSettings {
                    no_redirect: Some(false),
                    no_log: Some(true),
                    no_cookie_jar: Some(false),
                },
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org"),
                    http_version: WithDefault::default()
                },
                body: model::RequestBody::None,
                pre_request_script: None,
                response_handler: Some(ResponseHandler::Script(
                    response_handler_script.to_string()
                )),
                save_response: None
            }
        );
    }

    #[test]
    pub fn has_valid_extension() {
        // ok
        assert!(Parser::has_valid_extension(&"test.rest"));
        assert!(Parser::has_valid_extension(&"rest.http"));

        assert!(Parser::has_valid_extension(&"C:\\folder\\test.rest"));
        assert!(Parser::has_valid_extension(&"/home/user/test.rest"));

        assert!(Parser::has_valid_extension(&std::path::Path::new(
            "test.rest"
        )));

        assert!(Parser::has_valid_extension(&std::path::Path::new(
            "test.http"
        )));

        assert!(Parser::has_valid_extension(&std::path::Path::new(
            "C:\\folder\\test.rest"
        )));

        assert!(Parser::has_valid_extension(&std::path::Path::new(
            "/home/usr/folder/test.rest"
        )));

        // nok
        assert!(!Parser::has_valid_extension(&"test"));
        assert!(!Parser::has_valid_extension(&"/home/user/test"));
        assert!(!Parser::has_valid_extension(&""));
    }

    #[test]
    // https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1
    pub fn is_multipart_boundary_valid() {
        // at least one character is required
        let boundary = "";
        assert_eq!(Parser::is_multipart_boundary_valid(boundary).is_err(), true);

        // no more than 70 characters
        let boundary = "a".repeat(71);
        assert_eq!(
            Parser::is_multipart_boundary_valid(&boundary).is_err(),
            true
        );

        // at least one character is required
        let boundary = "a";

        assert_eq!(
            Parser::is_multipart_boundary_valid(&boundary).is_err(),
            false
        );

        // up to 70 characters is ok
        let boundary = "a".repeat(70);
        assert_eq!(
            Parser::is_multipart_boundary_valid(&boundary).is_err(),
            false
        );

        // no spaces within allowed
        let boundary = "a b";
        assert_eq!(
            Parser::is_multipart_boundary_valid(&boundary).is_err(),
            true
        );

        // these characters are allowed
        let boundary = "0123456789abcdefghijklmnopqrstuvwyxz";
        assert_eq!(
            Parser::is_multipart_boundary_valid(&boundary).is_err(),
            false
        );

        let boundary = "ABCDEFGHIJKLMNOPQRSTUVWXYZ'()+_,-./:=?";
        assert_eq!(
            Parser::is_multipart_boundary_valid(&boundary).is_err(),
            false
        );
    }

    #[test]
    pub fn parse_with_redirect_overwrite_response() {
        let str = r###"# @name=New Request
GET https://httpbin.org/get

>>! test.txt"###;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("New Request".to_string()),
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org/get"),
                    http_version: WithDefault::default()
                },
                save_response: Some(SaveResponse::RewriteFile(std::path::PathBuf::from(
                    "test.txt"
                ))),

                ..Default::default()
            }
        );
    }

    #[test]
    pub fn parse_with_redirect_new_file_response() {
        let str = r###"# @name=New Request
GET https://httpbin.org/get

>> test.txt"###;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            Request {
                name: Some("New Request".to_string()),
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org/get"),
                    http_version: WithDefault::default()
                },
                save_response: Some(SaveResponse::NewFileIfExists(std::path::PathBuf::from(
                    "test.txt"
                ))),

                ..Default::default()
            }
        );
    }

    #[test]
    /// If no boundary is given use default boundary '--boundary--'
    pub fn parse_multipart_no_boundary() {
        let str = r####"# @name=New Request
GET https://httpbin.org/{{abc}}
Content-Type: multipart/form-data

--boundary--

>>! test.txt"####;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        // should have one error warning that no boundary was given
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0].details[0].error,
            ParseError::MissingMultipartHeaderBoundaryDefinition(_)
        ));
        //assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 0);
        assert_eq!(
            Into::<Request>::into(errs[0].partial_request.clone()),
            Request {
                name: Some("New Request".to_string()),
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org/{{abc}}"),
                    http_version: WithDefault::default()
                },
                headers: vec![Header::new("Content-Type", "multipart/form-data")],
                body: RequestBody::Multipart {
                    boundary: "--boundary--".to_string(),
                    parts: vec![]
                },
                save_response: Some(SaveResponse::RewriteFile(std::path::PathBuf::from(
                    "test.txt"
                ))),

                ..Default::default()
            }
        );
    }

    #[test]
    pub fn parse_multipart_single_boundary_no_filename() {
        let str = r###"# @name=New Request
GET https://httpbin.org/{{abc}}
Content-Type: multipart/form-data; boundary="--boundary--"

----boundary--
Content-Disposition: form-data; name=""


----boundary----"###;

        let FileParseResult { requests, errs } = Parser::parse(str, false);
        // one error allowed, name should not be empty of content-disposition inside a multipart
        assert_eq!(errs.len(), 1);
        //assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 0);
        assert_eq!(
            Into::<Request>::into(errs[0].partial_request.clone()),
            Request {
                name: Some("New Request".to_string()),
                request_line: RequestLine {
                    method: WithDefault::Some(HttpMethod::GET),
                    target: RequestTarget::from("https://httpbin.org/{{abc}}"),
                    http_version: WithDefault::default()
                },
                headers: vec![Header::new(
                    "Content-Type",
                    "multipart/form-data; boundary=\"--boundary--\""
                )],
                body: RequestBody::Multipart {
                    boundary: "--boundary--".to_string(),
                    parts: vec![Multipart {
                        disposition: DispositionField::new(""),
                        headers: vec![],
                        data: DataSource::Raw("".to_string())
                    }]
                },
                ..Default::default()
            }
        );
    }

    #[test]
    pub fn parse_with_content_type_and_empty_body() {
        let str = r####"
POST https://test.com/formEncoded
Content-Type: application/json
"####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(
            request.headers,
            vec![Header::new("Content-Type", "application/json")]
        );

        assert_eq!(
            request.body,
            RequestBody::Raw {
                data: DataSource::Raw(String::new())
            }
        );

        let str = r####"
POST https://test.com/formEncoded
"####;

        let FileParseResult { mut requests, errs } = Parser::parse(str, false);
        assert_eq!(errs, vec![]);
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0);

        assert_eq!(request.headers, vec![]);

        assert_eq!(request.body, RequestBody::None);
    }
}

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{ClientBuilder, Proxy, Url};
use reqwest::header::HeaderMap;
use reqwest::multipart::{Form, Part};
use reqwest::redirect::Policy;
use tokio::task;

use crate::app::app::App;
use crate::panic_error;
use crate::request::auth::Auth::{BasicAuth, BearerToken, NoAuth};
use crate::request::body::{ContentType, find_file_format_in_content_type};

impl App<'_> {
    pub async fn send_request(&mut self) {
        let local_selected_request = self.get_selected_request_as_local();

        {
            let mut selected_request = local_selected_request.write().unwrap();

            // Avoid creating more than one thread
            if selected_request.is_pending {
                return;
            }

            let mut client_builder = ClientBuilder::new()
                .default_headers(HeaderMap::new())
                .referer(false);

            /* REDIRECTS */

            if !selected_request.settings.allow_redirects {
                client_builder = client_builder.redirect(Policy::none());
            }

            /* STORE COOKIES */

            let should_store_cookies = selected_request.settings.store_received_cookies;

            client_builder = client_builder.cookie_store(should_store_cookies);

            /* PROXY */

            if selected_request.settings.use_config_proxy {
                match &self.config.proxy {
                    None => {}
                    Some(proxy) => {
                        match &proxy.http_proxy {
                            None => {}
                            Some(http_proxy_str) => {
                                let proxy = match Proxy::http(http_proxy_str) {
                                    Ok(proxy) => proxy,
                                    Err(e) => panic_error(format!("Could not parse HTTP proxy\n\t{e}"))
                                };
                                client_builder = client_builder.proxy(proxy);
                            }
                        }

                        match &proxy.https_proxy {
                            None => {}
                            Some(https_proxy_str) => {
                                let proxy = match Proxy::https(https_proxy_str) {
                                    Ok(proxy) => proxy,
                                    Err(e) => panic_error(format!("Could not parse HTTPS proxy\n\t{e}"))
                                };
                                client_builder = client_builder.proxy(proxy);
                            }
                        }
                    }
                }
            }

            /* COOKIES */

            let local_cookie_store = Arc::clone(&self.cookies_popup.cookie_store);
            client_builder = client_builder.cookie_provider(local_cookie_store);

            /* CLIENT */

            let client = client_builder.build().expect("Could not build HTTP client");

            /* PARAMS */

            let params = self.key_value_vec_to_tuple_vec(&selected_request.params);

            /* URL */

            let url = self.replace_env_keys_by_value(&selected_request.url);

            let url = match Url::parse_with_params(&url, params) {
                Ok(url) => url,
                Err(_) => {
                    selected_request.result.status_code = Some(String::from("INVALID URL"));
                    return;
                }
            };

            /* REQUEST */

            let mut request = client.request(
                selected_request.method.to_reqwest(),
                url
            );

            /* CORS */
            
            if self.config.disable_cors.unwrap_or(false) {
                request = request.fetch_mode_no_cors();
            }
            
            /* AUTH */

            match &selected_request.auth {
                NoAuth => {}
                BasicAuth(username, password) => {
                    let username = self.replace_env_keys_by_value(username);
                    let password = self.replace_env_keys_by_value(password);

                    request = request.basic_auth(username, Some(password));
                }
                BearerToken(bearer_token) => {
                    let bearer_token = self.replace_env_keys_by_value(bearer_token);

                    request = request.bearer_auth(bearer_token);
                }
            }

            /* BODY */

            match &selected_request.body {
                ContentType::NoBody => {},
                ContentType::Multipart(form_data) => {
                    let mut multipart = Form::new();

                    for form_data in form_data {
                        let key = self.replace_env_keys_by_value(&form_data.data.0);
                        let value = self.replace_env_keys_by_value(&form_data.data.1);

                        // If the value starts with !!, then it is supposed to be a file
                        if value.starts_with("!!") {
                            let path = PathBuf::from(&value[2..]);

                            match get_file_content_with_name(path) {
                                Ok((file_content, file_name)) => {
                                    let part = Part::bytes(file_content).file_name(file_name);
                                    multipart = multipart.part(key, part);
                                }
                                Err(_) => {
                                    selected_request.result.status_code = Some(String::from("COULD NOT OPEN FILE"));
                                    return;
                                }
                            }
                        }
                        else {
                            multipart = multipart.text(key, value);
                        }
                    }

                    request = request.multipart(multipart);
                },
                ContentType::Form(form_data) => {
                    let form = self.key_value_vec_to_tuple_vec(form_data);

                    request = request.form(&form);
                },
                ContentType::File(file_path) => {
                    let file_path_with_env_values = self.replace_env_keys_by_value(file_path);
                    let path = PathBuf::from(file_path_with_env_values);

                    match tokio::fs::File::open(path).await {
                        Ok(file) => {
                            request = request.body(file);
                        }
                        Err(_) => {
                            selected_request.result.status_code = Some(String::from("COULD NOT OPEN FILE"));
                            return;
                        }
                    }
                },
                ContentType::Raw(body) | ContentType::Json(body) | ContentType::Xml(body) | ContentType::Html(body) | ContentType::Javascript(body) => {
                    request = request.body(body.to_string());
                }
            };

            /* HEADERS */

            for header in &selected_request.headers {
                if !header.enabled {
                    continue;
                }

                let header_name = self.replace_env_keys_by_value(&header.data.0);
                let header_value = self.replace_env_keys_by_value(&header.data.1);

                request = request.header(header_name, header_value);
            }

            let local_selected_request = self.get_selected_request_as_local();
            let local_last_highlighted = Arc::clone(&self.syntax_highlighting.last_highlighted);

            /* SEND REQUEST */

            task::spawn(async move {
                local_selected_request.write().unwrap().is_pending = true;

                let request_start = Instant::now();
                let elapsed_time: Duration;

                match request.send().await {
                    Ok(response) => {
                        let status_code = response.status().to_string();

                        let headers: Vec<(String, String)> = response.headers().clone()
                            .iter()
                            .map(|(header_name, header_value)| {
                                let value = header_value.to_str().unwrap_or("").to_string();

                                (header_name.to_string(), value)
                            })
                            .collect();

                        let cookies = response.cookies()
                            .map(|cookie| {
                                format!("{}: {}", cookie.name(), cookie.value())
                            })
                            .collect::<Vec<String>>()
                            .join("\n");

                        let mut result_body = response.text().await.unwrap();

                        // If the request response content can be pretty printed
                        if local_selected_request.read().unwrap().settings.pretty_print_response_content {
                            // If a file format has been found in the content-type header
                            if let Some(file_format) = find_file_format_in_content_type(&headers) {
                                // Match the file format
                                match file_format.as_str() {
                                    "json" => {
                                        result_body = jsonxf::pretty_print(&result_body).unwrap_or(result_body);
                                    },
                                    _ => {}
                                }
                            }
                        }
                        
                        {
                            let mut selected_request = local_selected_request.write().unwrap();
                            selected_request.result.status_code = Some(status_code);
                            selected_request.result.body = Some(result_body);
                            selected_request.result.cookies = Some(cookies);
                            selected_request.result.headers = headers;
                        }
                        
                    },
                    Err(error) => {
                        let response_status_code;

                        if let Some(status_code) = error.status() {
                            response_status_code = Some(status_code.to_string());
                        } else {
                            response_status_code = None;
                        }
                        let result_body = error.to_string();


                        {
                            let mut selected_request = local_selected_request.write().unwrap();
                            selected_request.result.status_code = response_status_code;
                            selected_request.result.body = Some(result_body);
                            selected_request.result.cookies = None;
                            selected_request.result.headers = vec![];
                        }
                    }
                };

                elapsed_time = request_start.elapsed();
                local_selected_request.write().unwrap().result.duration = Some(format!("{:?}", elapsed_time));

                local_selected_request.write().unwrap().is_pending = false;
                *local_last_highlighted.write().unwrap() = None;
            });
        }
    }
}

pub fn get_file_content_with_name(path: PathBuf) -> std::io::Result<(Vec<u8>, String)> {
    let mut buffer: Vec<u8> = vec![];
    let mut file = File::open(path.clone())?;

    file.read_to_end(&mut buffer)?;

    let file_name = path.file_name().unwrap().to_str().unwrap();

    return Ok((buffer, file_name.to_string()));
}
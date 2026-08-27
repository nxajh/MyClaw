use super::*;
// ── API client ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ApiClient {
    pub(crate) api_base: String,
    pub(crate) http: Client,
    pub(crate) state: Arc<RwLock<SharedState>>,
    pub(crate) client_version: String,
    pub(crate) account_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct UploadedMediaInfo {
    pub(crate) filekey: String,
    pub(crate) download_encrypted_query_param: String,
    pub(crate) aeskey_hex: String,
    pub(crate) file_size: i64,
    pub(crate) file_size_ciphertext: i64,
}

impl ApiClient {
    pub(crate) fn new(config: &WechatAccountConfig, account_id: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(config.poll_timeout + 15))
            .build()
            .unwrap_or_else(|_| Client::new());
        let mut state = SharedState::default();
        if let Some(ref token) = config.bot_token {
            state.bot_token = Some(token.clone());
        }
        if let Some(ref key) = config.aes_key {
            state.aes_key = Some(key.clone());
        }
        // Restore persisted context tokens
        state.context_tokens = load_context_tokens();
        // Restore the persisted get_updates cursor so a restart resumes
        // polling instead of re-pulling history (kill -9 during polling
        // used to lose the in-memory cursor entirely).
        state.get_updates_buf = load_get_updates_buf(&account_id);
        Self {
            api_base: config.api_base.trim_end_matches('/').to_string(),
            http,
            state: Arc::new(RwLock::new(state)),
            client_version: build_client_version().to_string(),
            account_id,
        }
    }

    pub(crate) fn url(&self, endpoint: &str) -> String {
        let base = self
            .state
            .read()
            .api_base
            .clone()
            .unwrap_or_else(|| self.api_base.clone());
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        )
    }

    pub(crate) fn random_uin_header() -> String {
        let uin: u32 = rand::random();
        BASE64.encode(uin.to_string())
    }

    pub(crate) async fn api_post(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let mut req = self.http.post(self.url(endpoint));
        req = req.header("AuthorizationType", "ilink_bot_token");
        if let Some(token) = self
            .state
            .read()
            .bot_token
            .clone()
            .filter(|t| !t.is_empty())
        {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req = req
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);

        let resp = req
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        resp.json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))
    }

    pub(crate) async fn api_get(&self, endpoint: &str) -> Result<serde_json::Value, ApiError> {
        let req = self
            .http
            .get(self.url(endpoint))
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);

        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        resp.json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))
    }

    pub(crate) fn check_ret(&self, raw: &serde_json::Value) -> Result<(), ApiError> {
        let code = raw.get("ret").and_then(|v| v.as_i64()).unwrap_or(0);
        let errmsg = raw.get("errmsg").and_then(|v| v.as_str()).unwrap_or("");
        match code {
            0 => Ok(()),
            -14 => Err(ApiError::Api(-14, "rate limited".into())),
            _ if code != 0 => Err(ApiError::Api(code, errmsg.into())),
            _ => Ok(()),
        }
    }

    // ── High-level API methods ──────────────────────────────────────────

    pub(crate) async fn get_updates(&self) -> Result<GetUpdatesResponse, ApiError> {
        let buf = self.state.read().get_updates_buf.clone();
        let req_body = GetUpdatesRequest {
            get_updates_buf: buf,
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/getupdates",
                &serde_json::to_value(&req_body).unwrap(),
            )
            .await?;

        let parsed: GetUpdatesResponse = serde_json::from_value(resp.clone())
            .map_err(|e| ApiError::Parse(format!("get_updates: {e}")))?;

        if parsed.ret != 0 || parsed.errcode != 0 {
            return Err(ApiError::Api(
                if parsed.errcode != 0 {
                    parsed.errcode
                } else {
                    parsed.ret
                },
                if parsed.errmsg.is_empty() {
                    "get_updates error".into()
                } else {
                    parsed.errmsg
                },
            ));
        }

        let new_buf = parsed.get_updates_buf.as_str();
        if !new_buf.is_empty() {
            persist_get_updates_buf(&self.account_id, new_buf);
            self.state.write().get_updates_buf = new_buf.to_string();
        }
        Ok(parsed)
    }

    pub(crate) async fn send_text(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<(), ApiError> {
        let client_id = format!("myclaw_{}", uuid::Uuid::new_v4());
        let run_id = self.state.read().run_ids.get(to_user_id).cloned();
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: Some(SendTextItem {
                        text: filter_markdown(text),
                    }),
                    image_item: None,
                    video_item: None,
                    file_item: None,
                }],
                context_token: context_token.map(String::from),
                run_id,
            },
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/sendmessage",
                &serde_json::to_value(&req).unwrap(),
            )
            .await?;
        self.check_ret(&resp)
    }

    pub(crate) async fn send_typing(&self, to_user_id: &str, typing: bool) -> Result<(), ApiError> {
        let ticket = self
            .state
            .read()
            .typing_tickets
            .get(to_user_id)
            .map(|(t, _)| t.clone())
            .unwrap_or_default();
        let req = SendTypingRequest {
            ilink_user_id: to_user_id.to_string(),
            typing_ticket: ticket,
            status: if typing {
                TYPING_STATUS_TYPING
            } else {
                TYPING_STATUS_CANCEL
            },
            base_info: build_base_info(),
        };
        let resp = self
            .api_post("ilink/bot/sendtyping", &serde_json::to_value(&req).unwrap())
            .await?;
        self.check_ret(&resp)
    }

    /// Send a tool call progress item (TOOL_CALL_START or TOOL_CALL_RESULT).
    pub(crate) async fn send_tool_progress(
        &self,
        to_user_id: &str,
        item_type: i64,
        tool_name: &str,
        tool_call_id: &str,
        status: Option<&str>,
        context_token: Option<&str>,
    ) -> Result<(), ApiError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut item_json = serde_json::json!({
            "type": item_type,
            "create_time_ms": now_ms,
        });
        match item_type {
            ITEM_TYPE_TOOL_CALL_START => {
                item_json["is_completed"] = serde_json::json!(false);
                item_json["tool_call_start_item"] = serde_json::json!({
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                });
            }
            ITEM_TYPE_TOOL_CALL_RESULT => {
                item_json["is_completed"] = serde_json::json!(true);
                item_json["tool_call_result_item"] = serde_json::json!({
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "status": status.unwrap_or("completed"),
                });
            }
            _ => return Err(ApiError::Parse("invalid tool item type".into())),
        }
        // Reference implementation uses message_state=FINISH(2) for tool progress,
        // and omits context_token when it's None (rather than sending null).
        let run_id = self.state.read().run_ids.get(to_user_id).cloned();
        let mut msg = serde_json::json!({
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": format!("myclaw_{}", uuid::Uuid::new_v4()),
            "message_type": MESSAGE_TYPE_BOT,
            "message_state": MESSAGE_STATE_FINISH,
            "item_list": [item_json],
        });
        if let Some(ct) = context_token {
            msg["context_token"] = serde_json::json!(ct);
        }
        if let Some(rid) = run_id {
            msg["run_id"] = serde_json::json!(rid);
        }
        let req = serde_json::json!({
            "msg": msg,
            "base_info": build_base_info(),
        });
        // sendmessage for tool items returns empty body (200, Content-Length: 0),
        // so we can't use api_post which calls resp.json(). Just check HTTP status.
        let mut http_req = self.http.post(self.url("ilink/bot/sendmessage"));
        http_req = http_req.header("AuthorizationType", "ilink_bot_token");
        if let Some(token) = self
            .state
            .read()
            .bot_token
            .clone()
            .filter(|t| !t.is_empty())
        {
            http_req = http_req.header("Authorization", format!("Bearer {token}"));
        }
        http_req = http_req
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);
        let resp = http_req
            .json(&req)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn get_config(&self, ilink_user_id: &str) -> Result<GetConfigResponse, ApiError> {
        let req = GetConfigRequest {
            ilink_user_id: ilink_user_id.to_string(),
            base_info: build_base_info(),
        };
        let resp = self
            .api_post("ilink/bot/getconfig", &serde_json::to_value(&req).unwrap())
            .await?;
        self.check_ret(&resp)?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_config: {e}")))
    }

    // ── CDN media methods ──────────────────────────────────────────────

    /// Parse CDN aes_key (base64) into raw 16-byte key.
    pub(crate) fn parse_cdn_aes_key(aes_key_base64: &str) -> Result<Vec<u8>, ApiError> {
        if aes_key_base64.is_empty() {
            return Err(ApiError::Parse("empty aes_key".into()));
        }
        let decoded = BASE64
            .decode(aes_key_base64.as_bytes())
            .map_err(|e| ApiError::Parse(format!("aes_key base64: {e}")))?;
        if decoded.len() == 16 {
            Ok(decoded)
        } else if decoded.len() == 32 {
            let hex_str = String::from_utf8_lossy(&decoded);
            hex::decode(hex_str.as_ref()).map_err(|e| ApiError::Parse(format!("aes_key hex: {e}")))
        } else {
            Err(ApiError::Parse(format!(
                "aes_key decoded to {} bytes, expected 16 or 32",
                decoded.len()
            )))
        }
    }

    /// Download and AES-128-ECB decrypt media from CDN.
    pub(crate) async fn download_cdn_media(
        &self,
        media: &CDNMedia,
        aeskey_hex: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        let key_bytes = if let Some(hex) = aeskey_hex.filter(|h| !h.is_empty()) {
            hex::decode(hex).map_err(|e| ApiError::Parse(format!("aeskey hex: {e}")))?
        } else {
            Self::parse_cdn_aes_key(&media.aes_key)?
        };
        if key_bytes.len() < 16 {
            return Err(ApiError::Parse("aes key too short".into()));
        }
        let key: [u8; 16] = key_bytes[..16].try_into().unwrap();

        let url = if !media.full_url.is_empty() {
            media.full_url.clone()
        } else {
            format!(
                "{}/download?encrypted_query_param={}",
                CDN_BASE_URL,
                urlencoding::encode(&media.encrypt_query_param)
            )
        };

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let ciphertext = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        decrypt_ecb(&ciphertext, &key).map_err(|e| ApiError::Parse(format!("decrypt: {e}")))
    }

    /// Compute AES-128-ECB ciphertext size (PKCS7 padding to 16-byte boundary).
    pub(crate) fn aes_ecb_padded_size(plaintext_size: usize) -> usize {
        ((plaintext_size / 16) + 1) * 16
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn get_upload_url(
        &self,
        filekey: &str,
        media_type: i64,
        to_user_id: &str,
        rawsize: i64,
        rawfilemd5: &str,
        filesize: i64,
        aeskey_hex: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let req = serde_json::json!({
            "filekey": filekey,
            "media_type": media_type,
            "to_user_id": to_user_id,
            "rawsize": rawsize,
            "rawfilemd5": rawfilemd5,
            "filesize": filesize,
            "no_need_thumb": true,
            "aeskey": aeskey_hex,
            "base_info": build_base_info(),
        });
        let resp = self.api_post("ilink/bot/getuploadurl", &req).await?;
        self.check_ret(&resp)?;
        Ok(resp)
    }

    /// Upload encrypted buffer to CDN, return download param.
    pub(crate) async fn upload_to_cdn(
        &self,
        plaintext: &[u8],
        upload_full_url: Option<&str>,
        upload_param: &str,
        filekey: &str,
        aes_key: &[u8; 16],
    ) -> Result<String, ApiError> {
        let ciphertext = encrypt_ecb(plaintext, aes_key);
        let url = if let Some(full) = upload_full_url.filter(|s| !s.trim().is_empty()) {
            full.to_string()
        } else {
            format!(
                "{}/upload?encrypted_query_param={}&filekey={}",
                CDN_BASE_URL,
                urlencoding::encode(upload_param),
                urlencoding::encode(filekey)
            )
        };
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let err_msg = resp
                .headers()
                .get("x-error-message")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown error");
            return Err(ApiError::Http(resp.status().as_u16(), err_msg.to_string()));
        }
        let download_param = resp
            .headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if let Some(param) = download_param {
            if !param.is_empty() {
                return Ok(param);
            }
        }
        // Fallback: some CDN deployments return a JSON body instead of the header.
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Parse(format!("cdn upload response: {e}")))?;
        let download_param = body
            .get("encrypt_query_param")
            .or_else(|| body.get("encrypted_query_param"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if download_param.is_empty() {
            return Err(ApiError::Parse("CDN upload: no download param".into()));
        }
        Ok(download_param)
    }

    /// Full media upload pipeline.
    pub(crate) async fn upload_media(
        &self,
        data: &[u8],
        to_user_id: &str,
        media_type: i64,
    ) -> Result<UploadedMediaInfo, ApiError> {
        use md5::{Digest, Md5};
        let rawsize = data.len() as i64;
        let mut hasher = Md5::new();
        hasher.update(data);
        let rawfilemd5: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let filesize = Self::aes_ecb_padded_size(data.len()) as i64;
        let filekey: String = (0..16)
            .map(|_| format!("{:02x}", rand::random::<u8>()))
            .collect();
        let aes_key_bytes: [u8; 16] = {
            let mut arr = [0u8; 16];
            for byte in arr.iter_mut() {
                *byte = rand::random();
            }
            arr
        };
        let aeskey_hex: String = aes_key_bytes.iter().map(|b| format!("{:02x}", b)).collect();

        let resp = self
            .get_upload_url(
                &filekey,
                media_type,
                to_user_id,
                rawsize,
                &rawfilemd5,
                filesize,
                &aeskey_hex,
            )
            .await?;
        let upload_full_url = resp
            .get("upload_full_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let upload_param = resp
            .get("upload_param")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let download_param = self
            .upload_to_cdn(
                data,
                upload_full_url.as_deref(),
                upload_param,
                &filekey,
                &aes_key_bytes,
            )
            .await?;

        Ok(UploadedMediaInfo {
            filekey,
            download_encrypted_query_param: download_param,
            aeskey_hex,
            file_size: rawsize,
            file_size_ciphertext: filesize,
        })
    }

    pub(crate) async fn get_bot_qrcode(&self) -> Result<QrCodeResponse, ApiError> {
        let resp = self.api_get("ilink/bot/get_bot_qrcode?bot_type=3").await?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_bot_qrcode: {e}")))
    }

    pub(crate) async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrStatus, ApiError> {
        let endpoint = format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            urlencoding::encode(qrcode)
        );
        let resp = self.api_get(&endpoint).await?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_qrcode_status: {e}")))
    }

    pub(crate) async fn notify_start(&self) -> Result<(), ApiError> {
        let req = serde_json::json!({
            "base_info": build_base_info(),
        });
        let resp = self.api_post("ilink/bot/msg/notifystart", &req).await?;
        self.check_ret(&resp)
    }

    pub(crate) async fn notify_stop(&self) -> Result<(), ApiError> {
        let req = serde_json::json!({
            "base_info": build_base_info(),
        });
        let resp = self.api_post("ilink/bot/msg/notifystop", &req).await?;
        self.check_ret(&resp)
    }
}

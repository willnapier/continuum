use anyhow::Result;
use continuum_mcp::*;
use rmcp::transport::stdio;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, ErrorData as McpError, Implementation, ProtocolVersion,
        ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

// Parameter structs for MCP tools

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchConversationsParams {
    #[schemars(description = "Search query to find in conversation messages")]
    query: String,
    #[schemars(description = "Filter by assistant (e.g., 'claude-code', 'codex', 'goose')")]
    assistant: Option<String>,
    #[schemars(description = "Filter by date (YYYY-MM-DD or YYYY-MM)")]
    date: Option<String>,
    #[schemars(description = "Maximum number of results (default: 20)")]
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SemanticSearchParams {
    #[schemars(
        description = "Search query describing the concept or topic to find in conversations"
    )]
    query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetTimelineParams {
    #[schemars(description = "Filter by date (YYYY-MM-DD or YYYY-MM). Defaults to today.")]
    date: Option<String>,
    #[schemars(description = "Filter by assistant (e.g., 'claude-code', 'codex', 'goose')")]
    assistant: Option<String>,
    #[schemars(description = "Maximum number of messages (default: 50)")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetConversationParams {
    #[schemars(description = "Session ID to retrieve")]
    session_id: String,
    #[schemars(description = "Assistant name (e.g., 'claude-code')")]
    assistant: String,
    #[schemars(description = "Date (YYYY-MM-DD)")]
    date: String,
}

#[derive(Clone)]
struct ContinuumServer {
    tool_router: ToolRouter<ContinuumServer>,
}

impl ContinuumServer {
    fn new() -> Result<Self> {
        // Verify continuum logs exist
        let _ = get_continuum_logs_path()?;

        info!("Initialized ContinuumServer");
        Ok(Self {
            tool_router: Self::tool_router(),
        })
    }
}

#[tool_router]
impl ContinuumServer {
    #[tool(
        description = "Search past conversations by keyword. Use when you know specific terms or phrases. For conceptual searches, use semantic_search_conversations instead."
    )]
    async fn search_conversations(
        &self,
        Parameters(params): Parameters<SearchConversationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let SearchConversationsParams {
            query,
            assistant,
            date,
            max_results,
        } = params;
        let max = max_results.unwrap_or(20);

        info!(
            "Searching conversations: assistant={:?}, date={:?}, max={}",
            assistant, date, max
        );

        let results = match search_conversations(&query, assistant.as_deref(), date.as_deref(), max)
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Search failed: {}", e);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Search failed: {}",
                    e
                ))]));
            }
        };

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matching conversations found. Try different search terms or check that conversations exist in ~/continuum-logs/".to_string()
            )]));
        }

        info!("Found {} matching messages", results.len());
        let json = serde_json::to_string_pretty(&results)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize results: {}\"}}", e));
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Semantic search using AI embeddings to find conceptually related conversations. PREFERRED for exploring topics, finding related discussions, or when exact keywords are unknown. Requires OPENAI_API_KEY."
    )]
    async fn semantic_search_conversations(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let SemanticSearchParams { query, max_results } = params;
        let max = max_results.unwrap_or(10);
        info!("Semantic search: max={}", max);

        let results = match semantic_search_conversations(&query, max) {
            Ok(r) => r,
            Err(e) => {
                warn!("Semantic search failed: {}", e);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Semantic search failed: {}. Check semantic-query-continuum, its config, host-local FAISS runtime, and OPENAI_API_KEY.",
                    e
                ))]));
            }
        };

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No semantically similar conversations found. Try a different query or broader terms.".to_string()
            )]));
        }

        info!("Found {} semantically similar conversations", results.len());
        let json = serde_json::to_string_pretty(&results)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize results: {}\"}}", e));
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Get conversation timeline by date and/or assistant. Returns chronological list of messages."
    )]
    async fn get_timeline(
        &self,
        Parameters(params): Parameters<GetTimelineParams>,
    ) -> Result<CallToolResult, McpError> {
        let GetTimelineParams {
            date,
            assistant,
            limit,
        } = params;
        let max = limit.unwrap_or(50);

        // Default to today if no date specified
        let date_str = date.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());

        info!(
            "Getting timeline: date={}, assistant={:?}, limit={}",
            date_str, assistant, max
        );

        let results = match get_timeline(Some(&date_str), assistant.as_deref(), max) {
            Ok(r) => r,
            Err(e) => {
                warn!("Timeline retrieval failed: {}", e);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Timeline retrieval failed: {}",
                    e
                ))]));
            }
        };

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No conversations found for date: {}. Check ~/continuum-logs/",
                date_str
            ))]));
        }

        info!("Retrieved {} messages", results.len());
        let json = serde_json::to_string_pretty(&results)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize timeline: {}\"}}", e));
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Get full conversation by session ID. Returns all messages from a specific conversation session."
    )]
    async fn get_conversation(
        &self,
        Parameters(params): Parameters<GetConversationParams>,
    ) -> Result<CallToolResult, McpError> {
        let GetConversationParams {
            session_id,
            assistant,
            date,
        } = params;
        info!(
            "Getting conversation: session={}, assistant={}, date={}",
            session_id, assistant, date
        );

        let logs_path = match get_continuum_logs_path() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to locate continuum logs: {}",
                    e
                ))]));
            }
        };

        let (messages, metadata) = match Archive::open(&logs_path)
            .and_then(|a| a.conversation(&assistant, &date, &session_id))
        {
            Ok(value) => value,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Conversation retrieval failed: {e}"
                ))]))
            }
        };

        #[derive(serde::Serialize)]
        struct ConversationResponse {
            session_id: String,
            assistant: String,
            date: String,
            metadata: Option<Session>,
            messages: Vec<Message>,
            message_count: usize,
        }

        let response = ConversationResponse {
            session_id,
            assistant,
            date,
            metadata,
            message_count: messages.len(),
            messages,
        };

        info!("Retrieved {} messages from session", response.message_count);
        let json = serde_json::to_string_pretty(&response).unwrap_or_else(|e| {
            format!("{{\"error\": \"Failed to serialize conversation: {}\"}}", e)
        });
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for ContinuumServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Continuum MCP Server - Search and retrieve past conversations across multiple AI assistants (Claude Code, Codex, Goose). \
                 Provides keyword search, semantic search, timeline browsing, and full conversation retrieval from plain-text JSONL logs."
                    .to_string()
            ),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing to stderr (stdout is reserved for MCP JSON protocol)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("continuum_mcp=info")),
        )
        .init();

    info!("Starting Continuum MCP Server v0.2.0");

    let server = ContinuumServer::new()?;

    info!("Server ready, listening on stdio");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}

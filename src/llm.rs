use crate::core::MessageSummary;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub binary: PathBuf,
    pub model: PathBuf,
    pub max_tokens: usize,
    pub temperature: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct LlmAnalysis {
    pub model: String,
    pub prompt: String,
    pub output: String,
}

impl LlmConfig {
    pub fn from_options(
        model: Option<PathBuf>,
        binary: Option<PathBuf>,
        max_tokens: usize,
        temperature: f32,
    ) -> Result<Self> {
        let model = model
            .or_else(|| env::var_os("SLIP_GGUF_MODEL").map(PathBuf::from))
            .or_else(|| {
                let local = PathBuf::from("models/Qwen3.5-4B-Q4_K_S.gguf");
                local.exists().then_some(local)
            })
            .context("set --model or SLIP_GGUF_MODEL to a local .gguf file")?;
        let binary = binary
            .or_else(|| env::var_os("SLIP_LLM_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("llama-completion"));
        Ok(Self {
            binary,
            model,
            max_tokens,
            temperature,
        })
    }

    pub fn analyze_message(
        &self,
        message: &MessageSummary,
        instruction: &str,
    ) -> Result<LlmAnalysis> {
        if !Path::new(&self.model).exists() {
            return Err(anyhow!("GGUF model not found: {}", self.model.display()));
        }
        let prompt = build_prompt(message, instruction);
        let output = Command::new(&self.binary)
            .arg("-m")
            .arg(&self.model)
            .arg("-sys")
            .arg(system_prompt())
            .arg("-p")
            .arg(&prompt)
            .arg("-n")
            .arg(self.max_tokens.to_string())
            .arg("--temp")
            .arg(format!("{:.2}", self.temperature))
            .arg("--single-turn")
            .arg("--no-display-prompt")
            .arg("--reasoning")
            .arg("off")
            .arg("--reasoning-budget")
            .arg("0")
            .arg("--no-warmup")
            .output()
            .with_context(|| format!("run local LLM binary {}", self.binary.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "local LLM failed with status {}: {}",
                output.status,
                stderr.trim()
            ));
        }

        let stdout = clean_llama_output(&String::from_utf8_lossy(&output.stdout));
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let text = if stdout.is_empty() {
            if stderr.is_empty() {
                "local model returned an empty response after cleaning".to_string()
            } else {
                stderr
            }
        } else {
            stdout
        };

        Ok(LlmAnalysis {
            model: self.model.display().to_string(),
            prompt: instruction.to_string(),
            output: text,
        })
    }
}

fn build_prompt(message: &MessageSummary, instruction: &str) -> String {
    let body = message
        .body
        .as_deref()
        .unwrap_or_default()
        .chars()
        .take(12_000)
        .collect::<String>();
    format!(
        "/no_think\n问题：{instruction}\n\n邮件元数据：\nSubject: {}\nFrom: {}\nTo: {}\nDate: {}\n\n邮件原文：\n{}\n\n直接给最终回答：",
        message.subject, message.from, message.to, message.date, body
    )
}

fn system_prompt() -> &'static str {
    "你是本地邮件阅读助手。只根据邮件原文回答，不使用外部知识。必须保持原意，不改写、歪曲、扩展邮件事实或原因。原文没有的信息回答“原文未提及”。语言极简，优先用中文短句；摘要最多3条。保留关键原文词句，不输出思考过程。不要补充“未提及其他细节”这类无信息句，除非用户问缺失信息。"
}

fn clean_llama_output(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value.replace("[end of text]", "");
    while let Some(start) = rest.find("<think>") {
        output.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find("</think>") {
            rest = rest[start + end + "</think>".len()..].to_string();
        } else {
            rest.clear();
            break;
        }
    }
    output.push_str(&rest);
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::clean_llama_output;

    #[test]
    fn clean_llama_output_removes_thinking_and_end_marker() {
        let value = "<think>\ninternal\n</think>\n\n1. 会议改至周五下午三点。\n2. 请携带合同草稿。 [end of text]\n";
        assert_eq!(
            clean_llama_output(value),
            "1. 会议改至周五下午三点。\n2. 请携带合同草稿。"
        );
    }
}

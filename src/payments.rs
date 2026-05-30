use std::collections::BTreeSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{Config, WorkerPrice};

#[derive(Debug, Clone)]
pub struct PaymentClient {
    cli_path: String,
    work_dir: std::path::PathBuf,
    engine: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMetadata {
    pub mint_url: String,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceEntry {
    pub mint_url: String,
    pub amount: u64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedPayment {
    pub mint_url: String,
    pub unit: String,
    pub amount: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentAuthorization {
    pub payment: ClaimedPayment,
    pub price_per_second: u64,
    pub prepaid_seconds: u64,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingOutcome {
    pub duration: u64,
    pub billable_duration: u64,
    pub cost: u64,
    pub change_amount: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CdkOutput {
    stdout: String,
    stderr: String,
}

impl PaymentClient {
    pub fn new(config: &Config) -> Self {
        Self {
            cli_path: config.cdk_cli_path.clone(),
            work_dir: config.cdk_work_dir.clone(),
            engine: config.cdk_engine.clone(),
        }
    }

    pub async fn check_pending_for_prices(&self, prices: &[WorkerPrice]) -> Result<()> {
        for unit in unique_units(prices) {
            self.run(&unit, ["check-pending"], None).await?;
        }
        Ok(())
    }

    pub async fn claim_payment(
        &self,
        token: &str,
        prices: &[WorkerPrice],
        signing_key: Option<&str>,
    ) -> Result<ClaimedPayment> {
        let metadata = self
            .decode_token(token, prices)
            .await
            .context("failed to decode Cashu payment token")?;
        find_price(prices, &metadata.mint_url, &metadata.unit)?;

        let before = self.balance(&metadata.unit).await?;
        let mut args = vec![
            "receive".to_string(),
            token.to_string(),
            "--allow-untrusted".to_string(),
        ];
        if let Some(signing_key) = signing_key {
            args.push("--signing-key".to_string());
            args.push(signing_key.to_string());
        }
        let output = self.run_owned(&metadata.unit, args, None).await?;

        let amount = if let Some(amount) =
            parse_received_amount(&output.stdout).or_else(|| parse_received_amount(&output.stderr))
        {
            amount
        } else {
            let after = self.balance(&metadata.unit).await?;
            received_delta(&before, &after, &metadata.mint_url, &metadata.unit)?
        };

        if amount == 0 {
            return Err(anyhow!("received payment amount was zero"));
        }

        Ok(ClaimedPayment {
            mint_url: metadata.mint_url,
            unit: metadata.unit,
            amount,
        })
    }

    pub async fn send_change(&self, mint_url: &str, unit: &str, amount: u64) -> Result<String> {
        if amount == 0 {
            return Err(anyhow!("change amount must be positive"));
        }

        let output = self
            .run_owned(
                unit,
                vec![
                    "send".to_string(),
                    "--mint-url".to_string(),
                    mint_url.to_string(),
                    "--amount".to_string(),
                    amount.to_string(),
                ],
                Some("\n"),
            )
            .await?;
        parse_cashu_token(&output.stdout)
            .or_else(|| parse_cashu_token(&output.stderr))
            .ok_or_else(|| anyhow!("cdk-cli send did not print a Cashu token"))
    }

    async fn decode_token(&self, token: &str, prices: &[WorkerPrice]) -> Result<TokenMetadata> {
        let unit = unique_units(prices)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("WORKER_PRICES must include at least one price"))?;
        let output = self
            .run_owned(
                &unit,
                vec!["decode-token".to_string(), token.to_string()],
                None,
            )
            .await?;
        parse_token_metadata(&output.stdout, prices)
            .or_else(|| parse_token_metadata(&output.stderr, prices))
            .ok_or_else(|| anyhow!("cdk-cli decode-token output did not include one mint and unit"))
    }

    async fn balance(&self, unit: &str) -> Result<Vec<BalanceEntry>> {
        let output = self.run(unit, ["balance"], None).await?;
        Ok(parse_balance_entries(&output.stdout))
    }

    async fn run<const N: usize>(
        &self,
        unit: &str,
        args: [&str; N],
        stdin: Option<&str>,
    ) -> Result<CdkOutput> {
        self.run_owned(
            unit,
            args.into_iter().map(|arg| arg.to_string()).collect(),
            stdin,
        )
        .await
    }

    async fn run_owned(
        &self,
        unit: &str,
        args: Vec<String>,
        stdin: Option<&str>,
    ) -> Result<CdkOutput> {
        let mut command = Command::new(&self.cli_path);
        command
            .arg("--work-dir")
            .arg(&self.work_dir)
            .arg("--engine")
            .arg(&self.engine)
            .arg("--unit")
            .arg(unit)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to execute {}", self.cli_path))?;

        if let Some(stdin_text) = stdin {
            let mut child_stdin = child.stdin.take().context("failed to open cdk-cli stdin")?;
            child_stdin
                .write_all(stdin_text.as_bytes())
                .await
                .context("failed to write cdk-cli stdin")?;
        }

        let output = child
            .wait_with_output()
            .await
            .context("failed waiting for cdk-cli")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            anyhow::bail!(
                "cdk-cli {} failed: {}{}",
                args.join(" "),
                stderr,
                if stdout.is_empty() {
                    ""
                } else {
                    stdout.as_str()
                }
            );
        }

        Ok(CdkOutput { stdout, stderr })
    }
}

pub fn authorize_payment(
    payment: ClaimedPayment,
    prices: &[WorkerPrice],
    min_duration: u64,
    max_duration: u64,
) -> Result<PaymentAuthorization> {
    let price = find_price(prices, &payment.mint_url, &payment.unit)?;
    let price_per_second = parse_price_per_second(price)?;
    let minimum = min_duration
        .checked_mul(price_per_second)
        .context("minimum payment overflowed")?;

    if payment.amount < minimum {
        return Err(anyhow!(
            "payment below minimum: received {} {}, required {} {}",
            payment.amount,
            payment.unit,
            minimum,
            payment.unit
        ));
    }

    let prepaid_seconds = payment.amount / price_per_second;
    let timeout_seconds = prepaid_seconds.min(max_duration);

    Ok(PaymentAuthorization {
        payment,
        price_per_second,
        prepaid_seconds,
        timeout: Duration::from_secs(timeout_seconds),
    })
}

pub fn settle_billing(
    authorization: &PaymentAuthorization,
    elapsed_seconds: u64,
    min_duration: u64,
) -> BillingOutcome {
    let billable_duration = elapsed_seconds.max(min_duration);
    let cost = billable_duration
        .saturating_mul(authorization.price_per_second)
        .min(authorization.payment.amount);
    BillingOutcome {
        duration: elapsed_seconds,
        billable_duration,
        cost,
        change_amount: authorization.payment.amount.saturating_sub(cost),
    }
}

fn find_price<'a>(
    prices: &'a [WorkerPrice],
    mint_url: &str,
    unit: &str,
) -> Result<&'a WorkerPrice> {
    prices
        .iter()
        .find(|price| price.mint_url == mint_url && price.unit == unit)
        .ok_or_else(|| anyhow!("no configured price for mint {mint_url} and unit {unit}"))
}

fn parse_price_per_second(price: &WorkerPrice) -> Result<u64> {
    let parsed = price
        .price_per_second
        .parse::<u64>()
        .context("WORKER_PRICES price_per_second must be a positive integer")?;
    if parsed == 0 {
        return Err(anyhow!(
            "WORKER_PRICES price_per_second must be a positive integer"
        ));
    }
    Ok(parsed)
}

fn unique_units(prices: &[WorkerPrice]) -> BTreeSet<String> {
    prices.iter().map(|price| price.unit.clone()).collect()
}

fn received_delta(
    before: &[BalanceEntry],
    after: &[BalanceEntry],
    mint_url: &str,
    unit: &str,
) -> Result<u64> {
    let before_amount = balance_amount(before, mint_url, unit);
    let after_amount = balance_amount(after, mint_url, unit);
    after_amount
        .checked_sub(before_amount)
        .ok_or_else(|| anyhow!("balance for mint {mint_url} {unit} did not increase"))
}

fn balance_amount(entries: &[BalanceEntry], mint_url: &str, unit: &str) -> u64 {
    entries
        .iter()
        .filter(|entry| entry.mint_url == mint_url && entry.unit == unit)
        .map(|entry| entry.amount)
        .sum()
}

pub fn parse_token_metadata(output: &str, prices: &[WorkerPrice]) -> Option<TokenMetadata> {
    let urls = unique_urls(output);
    if urls.len() != 1 {
        return None;
    }

    let units = unique_units(prices)
        .into_iter()
        .filter(|unit| output_mentions_unit(output, unit))
        .collect::<Vec<_>>();
    if units.len() != 1 {
        return None;
    }

    Some(TokenMetadata {
        mint_url: urls[0].clone(),
        unit: units[0].clone(),
    })
}

pub fn parse_received_amount(output: &str) -> Option<u64> {
    output
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            if !lower.contains("received") {
                return None;
            }
            first_u64(line)
        })
        .filter(|amount| *amount > 0)
}

pub fn parse_balance_entries(output: &str) -> Vec<BalanceEntry> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (index, rest) = line.split_once(':')?;
            if index.parse::<usize>().is_err() {
                return None;
            }

            let mut parts = rest.split_whitespace();
            let mint_url = parts.next()?.to_string();
            let amount = parts.next()?.parse::<u64>().ok()?;
            let unit = parts.next()?.to_string();
            Some(BalanceEntry {
                mint_url,
                amount,
                unit,
            })
        })
        .collect()
}

pub fn parse_cashu_token(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|part| {
        let token = part.trim_matches(|c: char| c == '"' || c == '\'' || c == ',' || c == ';');
        (token.starts_with("cashuA") || token.starts_with("cashuB")).then(|| token.to_string())
    })
}

fn unique_urls(output: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for prefix in ["https://", "http://"] {
        let mut rest = output;
        while let Some(start) = rest.find(prefix) {
            let candidate = &rest[start..];
            let end = candidate
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '"' | '\'' | ',' | ')' | ']' | '}' | '(' | '[' | '{')
                })
                .unwrap_or(candidate.len());
            let url = candidate[..end].trim_end_matches(['.', ';']).to_string();
            if !urls.contains(&url) {
                urls.push(url);
            }
            rest = &candidate[end..];
        }
    }
    urls
}

fn output_mentions_unit(output: &str, unit: &str) -> bool {
    if output.contains(&format!("\"{unit}\"")) || output.contains(&format!("'{unit}'")) {
        return true;
    }

    output
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == unit)
}

fn first_u64(value: &str) -> Option<u64> {
    value
        .split(|c: char| !c.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> Vec<WorkerPrice> {
        vec![WorkerPrice {
            mint_url: "https://mint.example".to_string(),
            price_per_second: "10".to_string(),
            unit: "sat".to_string(),
        }]
    }

    #[test]
    fn parses_decode_token_metadata() {
        let output = r#"
        TokenV4({
          "m": "https://mint.example",
          "u": "sat",
          "t": [{"p": [{"a": 10}]}]
        })
        "#;

        let metadata = parse_token_metadata(output, &prices()).unwrap();

        assert_eq!(
            metadata,
            TokenMetadata {
                mint_url: "https://mint.example".to_string(),
                unit: "sat".to_string(),
            }
        );
    }

    #[test]
    fn parses_receive_balance_and_send_outputs() {
        assert_eq!(parse_received_amount("Received: 42\n"), Some(42));

        let balances = parse_balance_entries(
            "Recovered 0 operations, 0 compensated, 0 skipped, 0 failed\n\
             0: https://mint.example 100 sat\n\
             \n\
             Total balance across all wallets: 100 sat\n",
        );
        assert_eq!(
            balances,
            vec![BalanceEntry {
                mint_url: "https://mint.example".to_string(),
                amount: 100,
                unit: "sat".to_string(),
            }]
        );

        assert_eq!(
            parse_cashu_token("\"cashuBpGF0gaJhaUgA...\"\n"),
            Some("cashuBpGF0gaJhaUgA...".to_string())
        );
    }

    #[test]
    fn pricing_math_covers_minimum_exact_overpay_and_cap() {
        let prices = prices();

        assert!(authorize_payment(
            ClaimedPayment {
                mint_url: "https://mint.example".to_string(),
                unit: "sat".to_string(),
                amount: 49,
            },
            &prices,
            5,
            120,
        )
        .is_err());

        let exact = authorize_payment(
            ClaimedPayment {
                mint_url: "https://mint.example".to_string(),
                unit: "sat".to_string(),
                amount: 50,
            },
            &prices,
            5,
            120,
        )
        .unwrap();
        assert_eq!(exact.prepaid_seconds, 5);
        assert_eq!(exact.timeout, Duration::from_secs(5));
        assert_eq!(settle_billing(&exact, 3, 5).cost, 50);

        let overpaid = authorize_payment(
            ClaimedPayment {
                mint_url: "https://mint.example".to_string(),
                unit: "sat".to_string(),
                amount: 2_000,
            },
            &prices,
            5,
            120,
        )
        .unwrap();
        assert_eq!(overpaid.prepaid_seconds, 200);
        assert_eq!(overpaid.timeout, Duration::from_secs(120));
        assert_eq!(
            settle_billing(&overpaid, 12, 5),
            BillingOutcome {
                duration: 12,
                billable_duration: 12,
                cost: 120,
                change_amount: 1_880,
            }
        );
    }

    #[test]
    #[ignore = "requires a real cdk-cli v0.16.0 binary and funded test mint configuration"]
    fn ignored_payment_integration_placeholder() {
        assert!(
            std::env::var("CDK_CLI_PATH").is_ok(),
            "set CDK_CLI_PATH and mint test configuration before running ignored payment tests"
        );
    }
}

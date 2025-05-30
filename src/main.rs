//use hf_hub::RepoType;
//use hf_hub::api::sync::Api;
//use hf_hub::Repo;
use hf_hub::{api::sync::Api, Repo, RepoType};
use clap::Parser;
use tokenizers::Tokenizer;
use anyhow::{Error as E, Result};
use std::io;
use std::io::Write;

use candle_transformers::models::mistral::{Config, Model as Mistral};
use candle_transformers::models::quantized_mistral::Model as QMistral;
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::generation::Sampling;
use candle_examples::token_output_stream::TokenOutputStream;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;

enum Model {
    Mistral(Mistral),
    Quantized(QMistral),
}

struct TextGeneration<'a, 'b> {
    model: &'a mut Model,
    device: &'b Device,
    tokenizer: TokenOutputStream,
    logits_processor: LogitsProcessor,
    repeat_penalty: f32,
    repeat_last_n: usize,
}

impl<'a, 'b, 'c> TextGeneration<'a, 'b> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        model: &'a mut Model,
        tokenizer: &'c Tokenizer,
        seed: u64,
        temp: Option<f64>,
        top_p: Option<f64>,
        top_k: Option<usize>,
        repeat_penalty: f32,
        repeat_last_n: usize,
        device: &'b Device,
    ) -> Self {
        let logits_processor = {
            let temperature = temp.unwrap_or(0.);
            let sampling = if temperature <= 0. {
                Sampling::ArgMax
            } else {
                match (top_k, top_p) {
                    (None, None) => Sampling::All { temperature },
                    (Some(k), None) => Sampling::TopK { k, temperature },
                    (None, Some(p)) => Sampling::TopP { p, temperature },
                    (Some(k), Some(p)) => Sampling::TopKThenTopP { k, p, temperature },
                }
            };
            LogitsProcessor::from_sampling(seed, sampling)
        };

        TextGeneration {
            model,
            tokenizer: TokenOutputStream::new(tokenizer.clone()),
            logits_processor,
            repeat_penalty,
            repeat_last_n,
            device,
        }
    }

    fn run(&mut self, prompt: &str, sample_len: usize) -> Result<()> {
        use std::io::Write;
        self.tokenizer.clear();
        let mut tokens = self
            .tokenizer
            .tokenizer()
            .encode(prompt, true)
            .map_err(E::msg)?
            .get_ids()
            .to_vec();
        for &t in tokens.iter() {
            if let Some(t) = self.tokenizer.next_token(t)? {
                print!("{t}")
            }
        }
        std::io::stdout().flush()?;

        let mut generated_tokens = 0usize;
        let eos_token = match self.tokenizer.get_token("</s>") {
            Some(token) => token,
            None => anyhow::bail!("cannot find the </s> token"),
        };
        let start_gen = std::time::Instant::now();
        for index in 0..sample_len {
            let context_size = if index > 0 { 1 } else { tokens.len() };
            let start_pos = tokens.len().saturating_sub(context_size);
            let ctxt = &tokens[start_pos..];
            let input = Tensor::new(ctxt, &self.device)?.unsqueeze(0)?;
            let logits = match &mut self.model {
                Model::Mistral(m) => m.forward(&input, start_pos)?,
                Model::Quantized(m) => m.forward(&input, start_pos)?,
            };
            let logits = logits.squeeze(0)?.squeeze(0)?.to_dtype(DType::F32)?;
            let logits = if self.repeat_penalty == 1. {
                logits
            } else {
                let start_at = tokens.len().saturating_sub(self.repeat_last_n);
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    self.repeat_penalty,
                    &tokens[start_at..],
                )?
            };

            let next_token = self.logits_processor.sample(&logits)?;
            tokens.push(next_token);
            generated_tokens += 1;
            if next_token == eos_token {
                break;
            }
            if let Some(t) = self.tokenizer.next_token(next_token)? {
                print!("{t}");
                std::io::stdout().flush()?;
            }
        }
        let dt = start_gen.elapsed();
        if let Some(rest) = self.tokenizer.decode_rest().map_err(E::msg)? {
            print!("{rest}");
        }
        std::io::stdout().flush()?;
        println!(
            "\n{generated_tokens} tokens generated ({:.2} token/s)",
            generated_tokens as f64 / dt.as_secs_f64(),
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Which {
    #[value(name = "7b-v0.1")]
    Mistral7bV01,
    #[value(name = "7b-v0.2")]
    Mistral7bV02,
    #[value(name = "7b-instruct-v0.1")]
    Mistral7bInstructV01,
    #[value(name = "7b-instruct-v0.2")]
    Mistral7bInstructV02,
    #[value(name = "7b-maths-v0.1")]
    Mathstral7bV01,
    #[value(name = "nemo-2407")]
    MistralNemo2407,
    #[value(name = "nemo-instruct-2407")]
    MistralNemoInstruct2407,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run on CPU rather than on GPU.
    #[arg(long)]
    cpu: bool,

    /// Enable tracing (generates a trace-timestamp.json file).
    #[arg(long)]
    tracing: bool,

    #[arg(long)]
    use_flash_attn: bool,

    #[arg(long, default_value = "")]
    prompt: String,

    /// The temperature used to generate samples.
    #[arg(long)]
    temperature: Option<f64>,

    /// Nucleus sampling probability cutoff.
    #[arg(long)]
    top_p: Option<f64>,

    /// Only sample among the top K samples.
    #[arg(long)]
    top_k: Option<usize>,

    /// The seed to use when generating random samples.
    #[arg(long, default_value_t = 299792458)]
    seed: u64,

    /// The length of the sample to generate (in tokens).
    #[arg(long, short = 'n', default_value_t = 10000)]
    sample_len: usize,

    /// The model size to use.
    #[arg(long, default_value = "7b-v0.1")]
    which: Which,

    #[arg(long)]
    model_id: Option<String>,

    #[arg(long, default_value = "main")]
    revision: String,

    #[arg(long)]
    tokenizer_file: Option<String>,

    #[arg(long)]
    config_file: Option<String>,

    #[arg(long)]
    weight_files: Option<String>,

    #[arg(long)]
    quantized: bool,

    /// Penalty to be applied for repeating tokens, 1. means no penalty.
    #[arg(long, default_value_t = 1.1)]
    repeat_penalty: f32,

    /// The context size to consider for the repeat penalty.
    #[arg(long, default_value_t = 64)]
    repeat_last_n: usize,

    /// Use the slower dmmv cuda kernel.
    #[arg(long)]
    force_dmmv: bool,
}

fn print_type_of<T> (_: &T) {
    println!("{}", std::any::type_name::<T>());
}

fn main() -> Result<()> {
    let args = Args::parse();

    println!(
        "avx: {}, neon: {}, simd128: {}, f16c: {}",
        candle_core::utils::with_avx(),
        candle_core::utils::with_neon(),
        candle_core::utils::with_simd128(),
        candle_core::utils::with_f16c()
    );

    let t_start = std::time::Instant::now();
    let api = Api::new()?;

    // model_id
    let model_id = match args.model_id {
        Some(model_id) => model_id,
        None => {
            if args.quantized {
                if args.which != Which::Mistral7bV01 {
                    anyhow::bail!("only 7b-v0.1 is available as a quantized model for now")
                }
                "lmz/candle-mistral".to_string()
            } else {
                let name = match args.which {
                    Which::Mistral7bV01 => "mistralai/Mistral-7B-v0.1",
                    Which::Mistral7bV02 => "mistralai/Mistral-7B-v0.2",
                    Which::Mistral7bInstructV01 => "mistralai/Mistral-7B-Instruct-v0.1",
                    Which::Mistral7bInstructV02 => "mistralai/Mistral-7B-Instruct-v0.2",
                    Which::Mathstral7bV01 => "mistralai/mathstral-7B-v0.1",
                    Which::MistralNemo2407 => "mistralai/Mistral-Nemo-Base-2407",
                    Which::MistralNemoInstruct2407 => "mistralai/Mistral-Nemo-Instruct-2407",
                };
                name.to_string()
            }
        }
    };

    // repo
    let repo = api.repo(Repo::with_revision(
        model_id,
        RepoType::Model,
        args.revision,
    ));

    // tokenizer_filename
    let tokenizer_filename = match args.tokenizer_file {
        Some(file) => std::path::PathBuf::from(file),
        None => repo.get("tokenizer.json")?,
    };

    // filenames
    let filenames = match args.weight_files {
        Some(files) => files
            .split(',')
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>(),
        None => {
            if args.quantized {
                vec![repo.get("model-q4k.gguf")?]
            } else {
                candle_examples::hub_load_safetensors(&repo, "model.safetensors.index.json")?
            }
        }
    };
    println!("retrieved the files in {:?}", t_start.elapsed());
    
    let t_start = std::time::Instant::now();
    // config
    let config = match args.config_file {
        Some(config_file) => serde_json::from_slice(&std::fs::read(config_file)?)?,
        None => {
            if args.quantized {
                Config::config_7b_v0_1(args.use_flash_attn)
            } else {
                let config_file = repo.get("config.json")?;
                serde_json::from_slice(&std::fs::read(config_file)?)?
            }
        }
    };

    // device
    let device = candle_examples::device(args.cpu)?;

    // model
    let (mut model, device) = if args.quantized {
        let filename = &filenames[0];
        let vb =
            candle_transformers::quantized_var_builder::VarBuilder::from_gguf(filename, &device)?;
        let model = QMistral::new(&config, vb)?;
        (Model::Quantized(model), device)
    } else {
        let dtype = if device.is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&filenames, dtype, &device)? };
        let model = Mistral::new(&config, vb)?;
        (Model::Mistral(model), device)
    };
    println!("loaded the model in {:?}", t_start.elapsed());

    // tokenizer
    let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(E::msg)?;
    
    let mut msg_in = String::new();
    loop {
        let mut pipeline = TextGeneration::new(
            &mut model,
            &tokenizer,
            args.seed,
            args.temperature,
            args.top_p,
            args.top_k,
            args.repeat_penalty,
            args.repeat_last_n,
            &device,
        );

        msg_in.clear();
        print!("> ");
        std::io::stdout().flush().unwrap();
        io::stdin().read_line(&mut msg_in).unwrap();
        let msg_cpy = msg_in.clone();
        //println!("echo {}", msg_cpy);
        pipeline.run(&msg_cpy, args.sample_len)?;
    }
    Ok(())
}

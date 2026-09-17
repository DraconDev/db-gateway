use std::{net::SocketAddr, path::PathBuf, process::ExitCode};
use clap::{Parser, Subcommand};
use serde_json::{Value,json};

#[derive(Parser)]
#[command(name="dg",version,about="DB Gateway — personal SQLite service")]
struct Cli {
    #[arg(long,global=true,default_value="http://127.0.0.1:7432")]
    server:String,
    /// File containing a raw credential (0600 on Unix). Never pass secrets as flags.
    #[arg(long,global=true)]
    key_file:Option<PathBuf>,
    /// Emit machine-readable JSON (also the current default).
    #[arg(long,global=true)]
    json:bool,
    #[command(subcommand)]
    command:Command,
}
#[derive(Subcommand)]
enum Command {
    /// Generate the owner locally; emits its credential once. Redirect securely.
    Init { #[arg(long)] data_dir:PathBuf },
    /// Run the authenticated server (loopback only in M1).
    Serve {
        #[arg(long,default_value="127.0.0.1:7432")] listen:SocketAddr,
        #[arg(long,default_value="state/dg")] data_dir:PathBuf,
    },
    List,
    Create { name:String },
    Query {
        name:String, sql:String,
        #[arg(long,default_value="[]")] args:String,
        #[arg(long)] idempotency_key:Option<String>,
    },
    /// Execute an atomic JSON batch from a file.
    Batch { name:String, file:PathBuf, #[arg(long)] idempotency_key:Option<String> },
    Key { #[command(subcommand)] command:KeyCommand },
}
#[derive(Subcommand)]
enum KeyCommand {
    Create { name:String, #[arg(long,value_parser=["database_read_only","database_read_write"])] role:String, #[arg(long,required=true)] database:Vec<String> },
    Revoke { name:String },
}
async fn run(cli:Cli) -> Result<(),dg::ServerError> {
    use reqwest::Method;
    use dg::{error::Error,ServerError::Application};
    let (method,path,body,idempotency)=match cli.command {
        Command::Serve{listen,data_dir}=>return dg::serve(listen,data_dir).await,
        Command::Init{data_dir}=>{
            let key=dg::store::Store::open(&data_dir).and_then(|store|store.bootstrap()).map_err(Application)?;
            println!("{}",json!({"owner_key":key}));
            return Ok(());
        },
        Command::List=>(Method::GET,"/v1/databases".into(),None,None),
        Command::Create{name}=>{dg::store::validate_name(&name).map_err(Application)?;(Method::POST,"/v1/databases".into(),Some(json!({"name":name})),None)},
        Command::Query{name,sql,args,idempotency_key}=>{
            dg::store::validate_name(&name).map_err(Application)?;
            let args:Vec<Value>=serde_json::from_str(&args).map_err(|_|Application(Error::invalid()))?;
            (Method::POST,format!("/v1/databases/{name}/query"),Some(json!({"sql":sql,"args":args})),idempotency_key)
        },
        Command::Batch{name,file,idempotency_key}=>{
            dg::store::validate_name(&name).map_err(Application)?;
            if std::fs::metadata(&file).map_err(|_|Application(Error::invalid()))?.len()>65536 {return Err(Application(Error::invalid()));}
            let body:dg::execution::Batch=serde_json::from_slice(&std::fs::read(file).map_err(|_|Application(Error::invalid()))?).map_err(|_|Application(Error::invalid()))?;
            (Method::POST,format!("/v1/databases/{name}/batch"),Some(serde_json::to_value(body).map_err(|_|Application(Error::invalid()))?),idempotency_key)
        },
        Command::Key{command:KeyCommand::Create{name,role,database}}=>(Method::POST,"/v1/keys".into(),Some(json!({"name":name,"role":role,"databases":database})),None),
        Command::Key{command:KeyCommand::Revoke{name}}=>{
            dg::store::validate_name(&name).map_err(Application)?;
            (Method::DELETE,format!("/v1/keys/{name}"),None,None)
        },
    };
    let key=dg::client::read_key(cli.key_file.as_deref().ok_or(Application(Error::new(400,"missing_credential","--key-file required")))?).map_err(Application)?;
    let result=dg::client::request(&cli.server,&key,method,&path,body,idempotency.as_deref()).await.map_err(Application)?;
    println!("{result}");
    if result.get("error").is_some(){return Err(Application(Error::new(400,"api_error","API request failed; see JSON response")));}
    Ok(())
}
#[tokio::main]
async fn main()->ExitCode {
    let cli=Cli::parse();
    tracing_subscriber::fmt().json().with_writer(std::io::stderr).init();
    match run(cli).await {
        Ok(())=>ExitCode::SUCCESS,
        Err(error)=>{tracing::error!(code=error.code(),message=%error,"command failed");ExitCode::FAILURE},
    }
}

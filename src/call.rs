/*
 * Copyright 2018-2021 TON DEV SOLUTIONS LTD.
 *
 * Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
 * this file except in compliance with the License.
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific TON DEV software governing permissions and
 * limitations under the License.
 */
use crate::config::Config;
use crate::convert;
use crate::helpers::{TonClient, now, now_ms, create_client_verbose, create_client_local, load_ton_address, load_abi, construct_account_from_tvc, query_account_field, TRACE_PATH, SDK_EXECUTION_ERROR_CODE};
use ton_abi::{Contract, ParamType};
use chrono::{TimeZone, Local};
use ton_client::abi::{
    encode_message,
    decode_message,
    ParamsOfDecodeMessage,
    ParamsOfEncodeMessage,
    Abi,
    FunctionHeader,
};
use ton_client::processing::{
    ParamsOfSendMessage,
    ParamsOfWaitForTransaction,
    ParamsOfProcessMessage,
    ProcessingEvent,
    wait_for_transaction,
    send_message,
};
use ton_client::tvm::{
    run_get,
    ParamsOfRunGet,
    run_executor,
    ParamsOfRunExecutor,
    AccountForExecutor,
    ExecutionOptions
};
use ton_block::{Account, Serializable, Deserializable, Message};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use serde_json::{Value, Map};
use ton_executor::{ExecuteParams, TransactionExecutor};
use ton_types::{HashmapE, UInt256};
use crate::debug::DebugLogger;
use crate::debug_executor::{DebugTransactionExecutor, TraceLevel};
use crate::message::{EncodedMessage, prepare_message_params, print_encoded_message, unpack_message};
use crate::replay::{CONFIG_ADDR, construct_blockchain_config};


async fn decode_call_parameters(ton: TonClient, msg: &EncodedMessage, abi: Abi) -> Result<(String, String), String> {
    let result = decode_message(
        ton,
        ParamsOfDecodeMessage {
            abi,
            message: msg.message.clone(),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| format!("couldn't decode message: {}", e))?;

    Ok((
        result.name,
        serde_json::to_string_pretty(
            &result.value.unwrap_or(json!({}))
        ).map_err(|e| format!("failed to serialize result: {}", e))?
    ))
}

fn parse_integer_param(value: &str) -> Result<String, String> {
    let value = value.trim_matches('\"');

    if value.ends_with('T') {
        convert::convert_token(value.trim_end_matches('T'))
    } else {
        Ok(value.to_owned())
    }
}

fn build_json_from_params(params_vec: Vec<&str>, abi: &str, method: &str) -> Result<String, String> {
    let abi_obj = Contract::load(abi.as_bytes()).map_err(|e| format!("failed to parse ABI: {}", e))?;
    let functions = abi_obj.functions();

    let func_obj = functions.get(method).ok_or("failed to load function from abi")?;
    let inputs = func_obj.input_params();

    let mut params_json = json!({ });
    for input in inputs {
        let mut iter = params_vec.iter();
        let _param = iter.find(|x| x.trim_start_matches('-') == input.name)
            .ok_or(format!(r#"argument "{}" of type "{}" not found"#, input.name, input.kind))?;

        let value = iter.next()
            .ok_or(format!(r#"argument "{}" of type "{}" has no value"#, input.name, input.kind))?
            .to_string();

        let value = match input.kind {
            ParamType::Uint(_) | ParamType::Int(_) => {
                json!(parse_integer_param(&value)?)
            },
            ParamType::Array(ref x) => {
                if let ParamType::Uint(_) = **x {
                    let mut result_vec: Vec<String> = vec![];
                    for i in value.split(|c| c == ',' || c == '[' || c == ']') {
                        if i != "" {
                            result_vec.push(parse_integer_param(i)?)
                        }
                    }
                    json!(result_vec)
                } else {
                    json!(value)
                }
            },
            _ => {
                json!(value)
            }
        };
        params_json[input.name.clone()] = value;
    }

    serde_json::to_string(&params_json).map_err(|e| format!("{}", e))
}

pub async fn emulate_locally(
    ton: TonClient,
    addr: &str,
    msg: String,
    is_fee: bool,
) -> Result<(), String> {
    let state: String;
    let state_boc = query_account_field(ton.clone(), addr, "boc").await;
    if state_boc.is_err() {
        if is_fee {
            let addr = ton_block::MsgAddressInt::from_str(addr)
                .map_err(|e| format!("couldn't decode address: {}", e))?;
            state = base64::encode(
                &ton_types::cells_serialization::serialize_toc(
                    &Account::with_address(addr)
                        .serialize()
                        .map_err(|e| format!("couldn't create dummy account for deploy emulation: {}", e))?
                ).map_err(|e| format!("failed to serialize account cell: {}", e))?
            );
        } else {
            return Err(state_boc.err().unwrap());
        }
    } else {
        state = state_boc.unwrap();
    }
    let res = run_executor(
        ton.clone(),
        ParamsOfRunExecutor {
            message: msg.clone(),
            account: AccountForExecutor::Account {
                boc: state,
                unlimited_balance: if is_fee {
                    Some(true)
                } else {
                    None
                },
            },
            ..Default::default()
        },
    )
    .await;

    if res.is_err() {
        return Err(format!("{:#}", res.err().unwrap()));
    }
    if is_fee {
        let fees = res.unwrap().fees;
        println!("{{");
        println!("  \"in_msg_fwd_fee\": \"{}\",", fees.in_msg_fwd_fee);
        println!("  \"storage_fee\": \"{}\",", fees.storage_fee);
        println!("  \"gas_fee\": \"{}\",", fees.gas_fee);
        println!("  \"out_msgs_fwd_fee\": \"{}\",", fees.out_msgs_fwd_fee);
        println!("  \"total_account_fees\": \"{}\",", fees.total_account_fees);
        println!("  \"total_output\": \"{}\"", fees.total_output);
        println!("}}");
    } else {
        println!("Local run succeeded. Executing onchain."); // TODO: check is_json
    }
    Ok(())
}

pub fn load_account(path: &str, from_tvc: bool) -> Result<Account, String> {
    Ok(if from_tvc {
        construct_account_from_tvc(path, None, None)?
    } else {
        Account::construct_from_file(path)
            .map_err(|e| format!(" failed to load account from the file {}: {}", path, e))?
    })
}

pub fn prepare_execution_options(bc_config: Option<&str>) -> Result<Option<ExecutionOptions>, String> {
    if let Some(config) = bc_config {
        let bytes = std::fs::read(config)
            .map_err(|e| format!("Failed to read data from file {}: {}", config, e))?;
        let config_boc = base64::encode(&bytes);
        let ex_opt = ExecutionOptions{
            blockchain_config: Some(config_boc),
            ..Default::default()
        };
        return Ok(Some(ex_opt));
    }
    Ok(None)
}


pub async fn send_message_and_wait(
    ton: TonClient,
    abi: Option<Abi>,
    msg: String,
    conf: Config,
) -> Result<serde_json::Value, String> {

    if !conf.is_json {
        println!("Processing... ");
    }
    let callback = |_| {
        async move {}
    };
    let result = send_message(
        ton.clone(),
        ParamsOfSendMessage {
            message: msg.clone(),
            abi: abi.clone(),
            send_events: false,
            ..Default::default()
        },
        callback,
    ).await
        .map_err(|e| format!("{:#}", e))?;

    if !conf.async_call {
        let result = wait_for_transaction(
            ton.clone(),
            ParamsOfWaitForTransaction {
                abi,
                message: msg.clone(),
                shard_block_id: result.shard_block_id,
                send_events: true,
                ..Default::default()
            },
            callback.clone(),
        ).await
            .map_err(|e| format!("{:#}", e))?;
        Ok(result.decoded.and_then(|d| d.output).unwrap_or(json!({})))
    } else {
        Ok(json!({}))
    }
}

pub async fn process_message(
    ton: TonClient,
    msg: ParamsOfEncodeMessage,
    config: Config,
) -> Result<serde_json::Value, String> {
    let callback = |event| { async move {
        match event {
            ProcessingEvent::DidSend { shard_block_id: _, message_id, message: _ } => println!("MessageId: {}", message_id),
            _ => (),
        }
    }};
    let res = if !config.is_json {
        ton_client::processing::process_message(
            ton.clone(),
            ParamsOfProcessMessage {
                message_encode_params: msg.clone(),
                send_events: true,
                ..Default::default()
            },
            callback,
        ).await
    } else {
        ton_client::processing::process_message(
            ton.clone(),
            ParamsOfProcessMessage {
                message_encode_params: msg.clone(),
                send_events: true,
                ..Default::default()
            },
            |_| { async move {} },
        ).await
    };

    if config.debug_fail && res.is_err()
        && res.clone().err().unwrap().code == SDK_EXECUTION_ERROR_CODE {
        if !config.is_json {
            println!("Execution failed. Starting debug...");
        }
        let acc_boc = query_account_field(
            ton.clone(),
            &msg.clone().address.unwrap(),
            "boc",
        ).await?;
        let mut account = Account::construct_from_base64(&acc_boc)
            .map_err(|e| format!("Failed to construct account: {}", e))?
            .serialize()
            .map_err(|e| format!("Failed to serialize account: {}", e))?;

        let config_acc = query_account_field(
            ton.clone(),
            CONFIG_ADDR,
            "boc",
        ).await?;

        let config_acc = Account::construct_from_base64(&config_acc)
            .map_err(|e| format!("Failed to construct config account: {}", e))?;
        let bc_config = construct_blockchain_config(&config_acc)?;

        let executor = Box::new(
            DebugTransactionExecutor::new(
                bc_config,
                None,
                TraceLevel::Minimal,
                false
            )
        );
        let message = encode_message(ton.clone(), msg).await
            .map_err(|e| format!("failed to create inbound message: {}", e))?;
        let message = Message::construct_from_base64(&message.message)
            .map_err(|e| format!("Faield to construct message: {}", e))?;

        let now = now_ms();
        let params = ExecuteParams {
            state_libs: HashmapE::default(),
            block_unixtime: (now / 1000) as u32,
            block_lt: now,
            last_tr_lt: Arc::new(AtomicU64::new(now)),
            seed_block: UInt256::default(),
            debug: true,
            ..ExecuteParams::default()
        };

        let trans = executor.execute_with_libs_and_params(
            Some(&message),
            &mut account,
            params
        );
        let msg_string = match trans {
            Ok(_trans) => {
                // decode_messages(trans.out_msgs,load_decode_abi(matches, config.clone())).await?;
                "Debug finished.".to_string()
            },
            Err(e) => {
                format!("Debug failed: {}", e)
            }
        };

        if !config.is_json {
            println!("{}", msg_string);
            println!("Log saved to {}", TRACE_PATH);
        }
    }

    let res = res.map_err(|e| format!("{:#}", e))?;

    Ok(res.decoded.and_then(|d| d.output).unwrap_or(json!({})))
}

pub async fn call_contract_with_result(
    conf: Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    is_fee: bool,
) -> Result<serde_json::Value, String> {
    let ton = create_client_verbose(&conf, !conf.debug_fail)?;
    if conf.debug_fail {
        log::set_max_level(log::LevelFilter::Trace);
        log::set_boxed_logger(
            Box::new(DebugLogger::new(TRACE_PATH.to_string()))
        ).map_err(|e| format!("Failed to set logger: {}", e))?;
    }
    call_contract_with_client(ton, conf, addr, abi, method, params, keys, is_fee).await
}

pub async fn call_contract_with_client(
    ton: TonClient,
    conf: Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    is_fee: bool,
) -> Result<serde_json::Value, String> {
    let abi = load_abi(&abi)?;

    let expire_at = conf.lifetime + now()?;
    let time = now_ms();
    let header = FunctionHeader {
        expire: Some(expire_at),
        time: Some(time),
        ..Default::default()
    };
    let msg_params = prepare_message_params(
        addr,
        abi.clone(),
        method,
        params,
        Some(header),
        keys.clone(),
    )?;

    let needs_encoded_msg = is_fee ||
        conf.async_call ||
        conf.local_run;

    if needs_encoded_msg {
        let msg = encode_message(ton.clone(), msg_params.clone()).await
            .map_err(|e| format!("failed to create inbound message: {}", e))?;

        if conf.local_run || is_fee {
            emulate_locally(ton.clone(), addr, msg.message.clone(), is_fee).await?;
            if is_fee {
                return Ok(Value::Null);
            }
        }
        if conf.async_call {
            return send_message_and_wait(ton,
                                         Some(abi),
                                         msg.message,
                                         conf).await;
        }
    }

    if !conf.is_json {
        print!("Expire at: ");
        let expire_at = Local.timestamp(expire_at as i64 , 0);
        println!("{}", expire_at.to_rfc2822());
    }
    process_message(ton.clone(), msg_params, conf).await
}

pub fn print_json_result(result: Value, conf: Config) -> Result<(), String> {
    if !result.is_null() {
        let result = serde_json::to_string_pretty(&result)
            .map_err(|e| format!("Failed to serialize the result: {}", e))?;
        if !conf.is_json {
            println!("Result: {}", result);
        } else {
            println!("{}", result);
        }
    }
    Ok(())
}

pub async fn call_contract(
    conf: Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    is_fee: bool,
) -> Result<(), String> {
    let result = call_contract_with_result(conf.clone(), addr, abi, method, params, keys, is_fee).await?;
    if !conf.is_json {
        println!("Succeeded.");
    }
    print_json_result(result, conf)?;
    Ok(())
}


pub async fn call_contract_with_msg(conf: Config, str_msg: String, abi: String) -> Result<(), String> {
    let ton = create_client_verbose(&conf, true)?;
    let abi = load_abi(&abi)?;

    let (msg, _) = unpack_message(&str_msg)?;
    if conf.is_json {
        println!("{{");
    }
    print_encoded_message(&msg, conf.is_json);

    let params = decode_call_parameters(ton.clone(), &msg, abi.clone()).await?;

    if !conf.is_json {
        println!("Calling method {} with parameters:", params.0);
        println!("{}", params.1);
        println!("Processing... ");
    } else {
        println!("  \"Method\": \"{}\",", params.0);
        println!("  \"Parameters\": {},", params.1);
        println!("}}");
    }
    let result = send_message_and_wait(ton, Some(abi), msg.message,  conf.clone()).await?;

    if !conf.is_json {
        println!("Succeeded.");
        if !result.is_null() {
            println!("Result: {}", serde_json::to_string_pretty(&result)
                .map_err(|e| format!("failed to serialize result: {}", e))?);
        }
    }
    Ok(())
}

pub fn parse_params(params_vec: Vec<&str>, abi: &str, method: &str) -> Result<String, String> {
    if params_vec.len() == 1 {
        // if there is only 1 parameter it must be a json string with arguments
        Ok(params_vec[0].to_owned())
    } else {
        build_json_from_params(params_vec, abi, method)
    }
}

pub async fn run_get_method(conf: Config, addr: &str, method: &str, params: Option<String>, is_local: bool, is_tvc: bool, bc_config: Option<&str>) -> Result<(), String> {
    let ton = if !is_local {
        create_client_verbose(&conf, true)?
    } else {
        create_client_local()?
    };

    let acc_boc = if is_local {
        let acc = load_account(addr, is_tvc)?;
        let acc_bytes = acc.write_to_bytes()
            .map_err(|e| format!("failed to load data from the account: {}", e))?;
        base64::encode(&acc_bytes)
    } else {
        let addr = load_ton_address(addr, &conf)
            .map_err(|e| format!("failed to parse address: {}", e.to_string()))?;
        query_account_field(ton.clone(), addr.as_str(), "boc").await?
    };

    let params = params.map(|p| serde_json::from_str(&p))
        .transpose()
        .map_err(|e| format!("arguments are not in json format: {}", e))?;

    if !conf.is_json {
        println!("Running get-method...");
    }
    let execution_options = prepare_execution_options(bc_config)?;
    let result = run_get(
        ton,
        ParamsOfRunGet {
            account: acc_boc,
            function_name: method.to_owned(),
            input: params,
            execution_options,
            ..Default::default()
        },
    ).await
    .map_err(|e| format!("run failed: {}", e.to_string()))?
    .output;

    if !conf.is_json {
        println!("Succeeded.");
        println!("Result: {}", result);
    } else {
        let mut res = Map::new();
        match result {
            Value::Array(array) => {
                let mut i = 0;
                for val in array.iter() {
                    res.insert(format!("value{}", i), val.to_owned());
                    i = 1 + i;
                }
            },
            _ => {
                res.insert("value0".to_owned(), result);
            }
        }
        let res = Value::Object(res);
        println!("{}", serde_json::to_string_pretty(&res).unwrap_or("Undefined".to_string()));
    }
    Ok(())
}

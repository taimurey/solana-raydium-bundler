use std::{io::Write, str::FromStr, sync::Arc};

use bincode::serialize;
use jito_protos::searcher::SubscribeBundleResultsRequest;
use jito_searcher_client::{get_searcher_client, send_bundle_with_confirmation};
use solana_address_lookup_table_program::state::AddressLookupTable;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    message::{v0::Message, VersionedMessage},
    native_token::lamports_to_sol,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use spl_associated_token_account::get_associated_token_address;

use crate::{
    env::{
        input::bundle_priority_tip,
        jito_auth::{auth_keypair, jito_tip_acc, jito_tip_inx},
        load_minter_settings,
    },
    raydium::{
        instruction::{
            decoder::SOLC_MINT,
            instruction::SOL_MINT,
            pool_ixs::{load_pool_keys, pool_ixs},
            swap_ixs::swap_ixs,
        },
        wallets::list_folders,
    },
};

#[derive(Debug, serde::Serialize)]
pub struct PoolDeployResponse {
    pub wallets: Vec<String>,
    pub amm_pool: Pubkey,
}

pub async fn pool_main() -> eyre::Result<()> {
    let (_, wallets) = match list_folders().await {
        Ok(folders) => folders,
        Err(e) => {
            eprintln!("Error listing folders: {}", e);
            return Ok(());
        }
    };

    let mut engine = load_minter_settings().await?;

    let rpc_client = Arc::new(RpcClient::new(engine.rpc_url.clone()));

    let deployer_key = Keypair::from_base58_string(&engine.deployer_key.clone());
    let buyer_key = Keypair::from_base58_string(&engine.buyer_key.clone());

    // -------------------Pool Creation Instructions--------------------------
    println!("Creating Pool Transaction");

    let (create_pool_ixs, amm_pool, amm_keys) = match pool_ixs(engine.clone()).await {
        Ok(ixs) => ixs,
        Err(e) => {
            eprintln!("Error creating pool IXs: {}", e);
            return Err(e);
        }
    };

    engine.pool_id = amm_pool.to_string();
    let mut file = std::fs::File::create("mintor_settings.json").unwrap();
    file.write_all(serde_json::to_string_pretty(&engine)?.as_bytes())?;

    let market_keys = load_pool_keys(rpc_client.clone(), amm_keys).await?;

    let bundle_tip = bundle_priority_tip().await;

    // -------------------LUT Account------------------------------------------

    let lut_creation = match Pubkey::from_str(&engine.lut_key) {
        Ok(lut) => lut,
        Err(e) => {
            panic!("LUT key not Found in Settings: {}", e);
        }
    };

    let mut raw_account = None;

    while raw_account.is_none() {
        match rpc_client.get_account(&lut_creation).await {
            Ok(account) => raw_account = Some(account),
            Err(e) => {
                eprintln!("Error getting LUT account: {}, retrying...", e);
            }
        }
    }

    let raw_account = raw_account.unwrap();

    let address_lookup_table = AddressLookupTable::deserialize(&raw_account.data)?;
    let address_lookup_table_account = AddressLookupTableAccount {
        key: lut_creation,
        addresses: address_lookup_table.addresses.to_vec(),
    };
    let market_keys = market_keys.clone();
    let server_data = engine.clone();

    let recent_blockhash = rpc_client.get_latest_blockhash().await?;

    //-------------------Pool Transaction---------------------------------------
    let versioned_msg = VersionedMessage::V0(
        Message::try_compile(
            &deployer_key.pubkey(),
            &create_pool_ixs, /* , tax_txn*/
            &[address_lookup_table_account.clone()],
            recent_blockhash,
        )
        .unwrap(),
    );

    let versioned_tx = match VersionedTransaction::try_new(versioned_msg, &[&deployer_key]) {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("Error creating pool transaction: {}", e);
            return Err(e.into());
        }
    };

    // -------------------Swap Instructions---------------------------------------

    let wallets_chunks = wallets.chunks(7).collect::<Vec<_>>();
    let mut txns_chunk = Vec::new();

    txns_chunk.push(versioned_tx);

    for (chunk_index, wallet_chunk) in wallets_chunks.iter().enumerate() {
        let mut current_instructions = Vec::new();
        let mut current_wallets = Vec::new();

        for (i, wallet) in wallet_chunk.iter().enumerate() {
            let user_token_source = get_associated_token_address(&wallet.pubkey(), &SOLC_MINT);

            let balance = match rpc_client
                .get_token_account_balance(&user_token_source)
                .await
            {
                Ok(balance) => balance.amount.parse::<u64>().unwrap(),
                Err(e) => {
                    eprintln!("Error getting token account balance: {}", e);
                    continue;
                }
            };

            println!("Balance: {} SOL", lamports_to_sol(balance));

            let user_token_source = get_associated_token_address(&wallet.pubkey(), &SOL_MINT);

            let swap_ixs = swap_ixs(
                server_data.clone(),
                amm_keys.clone(),
                market_keys.clone(),
                wallet,
                balance,
                false,
                user_token_source,
            )
            .unwrap();

            current_instructions.push(swap_ixs);
            current_wallets.push(wallet);

            if chunk_index == wallets_chunks.len() - 1 && i == wallet_chunk.len() - 1 {
                let tip = jito_tip_inx(buyer_key.pubkey(), jito_tip_acc(), bundle_tip);
                current_instructions.push(tip);
            }
        }

        println!("Tx-{}: {} wallets", chunk_index + 1, current_wallets.len());

        current_wallets.push(&buyer_key);

        let versioned_msg = VersionedMessage::V0(
            Message::try_compile(
                &buyer_key.pubkey(),
                &current_instructions,
                &[address_lookup_table_account.clone()],
                recent_blockhash,
            )
            .unwrap(),
        );

        let versioned_tx = match VersionedTransaction::try_new(versioned_msg, &current_wallets) {
            Ok(tx) => tx,
            Err(e) => {
                eprintln!("Error creating pool transaction: {}", e);
                panic!("Error: {}", e);
            }
        };

        txns_chunk.push(versioned_tx);
        // Now you can use chunk_index, current_wallets, and current_instructions
    }

    txns_chunk.iter().for_each(|tx| {
        println!("Txn: {:?}", tx.signatures);
    });

    let txn_size: Vec<_> = txns_chunk
        .iter()
        .map(|x| {
            let serialized_x = serialize(x).unwrap();
            serialized_x.len()
        })
        .collect();

    println!("txn_size: {:?}", txn_size);

    // -------------------Subscribe to Bundle Results---------------------------------------

    let mut client =
        get_searcher_client(&engine.block_engine_url, &Arc::new(auth_keypair())).await?;

    let mut bundle_results_subscription = client
        .subscribe_bundle_results(SubscribeBundleResultsRequest {})
        .await
        .expect("subscribe to bundle results")
        .into_inner();

    if txns_chunk.len() > 5 {
        eprintln!("Too many transactions to send in one bundle");
        return Err(eyre::eyre!("Too many transactions to send in one bundle"));
    }

    let rpc_client = &Arc::new(rpc_client);

    match send_bundle_with_confirmation(
        &txns_chunk,
        rpc_client,
        &mut client,
        &mut bundle_results_subscription,
    )
    .await
    {
        Ok(bundle_results) => bundle_results,
        Err(e) => {
            eprintln!("Error sending bundle: {}", e);
        }
    };

    Ok(())
}

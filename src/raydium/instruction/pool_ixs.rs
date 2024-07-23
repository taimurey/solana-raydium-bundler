use std::{str::FromStr, sync::Arc};

use crate::{
    env::{
        input::{liq_amount, token_percentage},
        PoolDataSettings,
    },
    raydium::instruction::{
        decoder::SOLC_MINT,
        instruction::{get_amm_pda_keys, initialize_amm_pool, SOL_MINT},
    },
};
use log::info;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction,
    native_token::{lamports_to_sol, sol_to_lamports},
    pubkey::{Pubkey, MAX_SEED_LEN},
    system_instruction::create_account_with_seed,
};

use solana_sdk::{signature::Keypair, signer::Signer};
use spl_token::instruction::initialize_account;

use super::instruction::{get_keys_for_market, AmmKeys, MarketPubkeys};

pub const AMM_PROGRAM: Pubkey = solana_sdk::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

pub async fn pool_ixs(
    pool_data: PoolDataSettings,
) -> eyre::Result<(Vec<Instruction>, Pubkey, AmmKeys)> {
    let market_program = Pubkey::from_str("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX")?;
    let market = Pubkey::from_str(&pool_data.market_id)?;
    let amm_coin_mint = Pubkey::from_str(&pool_data.token_mint)?;
    let amm_pc_mint = SOL_MINT;
    // maintnet: 7YttLkHDoNj9wyDur5pM1ejNaAvT9X4eqaYcHQqtj2G5
    // devnet: 3XMrhbv989VxAMi3DErLV9eJht1pHppW5LbKxe9fkEFR
    let create_fee_destination = Pubkey::from_str("7YttLkHDoNj9wyDur5pM1ejNaAvT9X4eqaYcHQqtj2G5")?;

    let wallet = Keypair::from_base58_string(&pool_data.deployer_key);

    let rpc_client = RpcClient::new(pool_data.rpc_url.clone());

    let associated_token = spl_associated_token_account::get_associated_token_address(
        &wallet.pubkey(),
        &Pubkey::from_str(&pool_data.token_mint)?,
    );

    let token_accounts = rpc_client
        .get_token_account_balance(&associated_token)
        .await?;

    let base_pc_amount = token_accounts.amount.parse::<u64>()?;

    println!("Base PC Amount: {}", base_pc_amount);

    let sol_amount = liq_amount();
    let percentage = token_percentage();

    let balance = rpc_client.get_balance(&wallet.pubkey()).await?;

    if balance < (sol_amount + sol_to_lamports(0.3 + 0.4)) {
        log::error!(
            "Insufficient balance in deployer key to create pool: {} SOL",
            lamports_to_sol(balance)
        );
        panic!();
    }

    let input_pc_amount = sol_to_lamports(lamports_to_sol(base_pc_amount) * percentage);

    // generate amm keys
    let amm_keys = get_amm_pda_keys(
        &AMM_PROGRAM,
        &market_program,
        &market,
        &amm_coin_mint,
        &amm_pc_mint,
    );

    let mut pool_inx = vec![];

    let (pubkey, seed) = generate_pubkey(wallet.pubkey()).await?;

    println!("Seed: {}", seed);

    let inx = create_account_with_seed(
        &wallet.pubkey(),
        &pubkey,
        &wallet.pubkey(),
        &seed,
        sol_amount + 2039280,
        165,
        &spl_token::id(),
    );

    let init = initialize_account(&spl_token::id(), &pubkey, &SOLC_MINT, &wallet.pubkey())?;

    let token = spl_associated_token_account::get_associated_token_address(
        &wallet.pubkey(),
        &amm_keys.amm_pc_mint,
    );

    println!("Token: {}", token);
    // build initialize instruction
    let build_init_instruction = initialize_amm_pool(
        &AMM_PROGRAM,
        &amm_keys,
        &create_fee_destination,
        &wallet.pubkey(),
        &spl_associated_token_account::get_associated_token_address(
            &wallet.pubkey(),
            &amm_keys.amm_coin_mint,
        ),
        &pubkey,
        &spl_associated_token_account::get_associated_token_address(
            &wallet.pubkey(),
            &amm_keys.amm_lp_mint,
        ),
        0,
        sol_amount,
        input_pc_amount,
    )?;

    pool_inx.push(inx);
    pool_inx.push(init);
    pool_inx.push(build_init_instruction);

    Ok((pool_inx, amm_keys.amm_pool, amm_keys))
}

pub async fn generate_pubkey(from_public_key: Pubkey) -> eyre::Result<(Pubkey, String)> {
    let seed = Keypair::new()
        .pubkey()
        .to_string()
        .chars()
        .take(MAX_SEED_LEN)
        .collect::<String>();
    info!("Seed: {}", seed);
    let public_key = Pubkey::create_with_seed(&from_public_key, &seed, &spl_token::id())?;
    info!("Public Key: {}", public_key);
    Ok((public_key, seed))
}

pub async fn load_pool_keys(
    rpc_client: Arc<RpcClient>,
    amm_keys: AmmKeys,
) -> eyre::Result<MarketPubkeys> {
    // load market keys
    let market_keys = get_keys_for_market(&rpc_client, &amm_keys.market_program, &amm_keys.market)
        .await
        .unwrap();

    Ok(market_keys)
}

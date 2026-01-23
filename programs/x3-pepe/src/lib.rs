#![allow(deprecated)]
use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{Mint, Token, TokenAccount, Transfer};

declare_id!("6hFfJP3EgJe8mL9rhBjXWhruJWHBaWnt8gvej14xDUuz");

#[program]
pub mod x3_pepe {
    use super::*;
    use anchor_spl::token;

    /// Initializes the X3MonsterBall program, setting up global config and the root user.
    pub fn initialize(
        ctx: Context<Initialize>,
        root: Pubkey,
        owner: Pubkey,
        prices: Vec<u64>,
    ) -> Result<()> {
        let global = &mut ctx.accounts.global_state;
        global.root = root;
        global.owner = owner;
        global.token_mint = ctx.accounts.token_mint.key();
        global.paused = false;
        global.max_level = prices.len() as u8;
        global.prices = prices;
        global.bump = ctx.bumps.global_state;
        global.root_bump = ctx.bumps.root_account;
        global.vault_token_account = ctx.accounts.vault_token_account.key();
        global.staking_token_account = ctx.accounts.staking_token_account.key();

        let root_acct = &mut ctx.accounts.root_account;
        root_acct.wallet = root;
        root_acct.referrer = Pubkey::default(); // root has no referrer (treated as top)
        root_acct.balance = 0;
        root_acct.matrix = Vec::with_capacity(global.max_level as usize);
        root_acct.closed_cycles = Vec::with_capacity(10);
        root_acct.bump = ctx.bumps.root_account;

        for _ in 0..global.max_level {
            root_acct.matrix.push(X3 {
                blocked: false,
                referrals: 0,
                cycles: 0,
                freeze: 0,
                level_bought_time: Clock::get()?.unix_timestamp as u64,
                close_level_time: 0,
            });
        }

        Ok(())
    }


    pub fn update_prices(
        ctx: Context<UpdatePrices>,
        prices: Vec<u64>,
    ) -> Result<()> {
        let global = &mut ctx.accounts.global_state;
        global.prices = prices;
        global.max_level = global.prices.len() as u8;
        let mut is_increase = false;
        while ctx.accounts.root_account.matrix.len() < global.max_level as usize {
            ctx.accounts.root_account.matrix.push(X3 {
                blocked: false,
                referrals: 0,
                cycles: 0,
                freeze: 0,
                level_bought_time: Clock::get()?.unix_timestamp as u64,
                close_level_time: 0,
            });
            is_increase = true;
        }
        if is_increase {
            ctx.accounts.root_account.to_account_info().resize(UserAccount::max_size(global.max_level as usize))?;
        }

        Ok(())
    }

    /// Updates global state configuration (owner and/or staking_token_account).
    /// Only the current owner can call this function.
    pub fn update_global(
        ctx: Context<UpdateGlobal>,
        new_owner: Option<Pubkey>,
        new_staking_token_account: Option<Pubkey>,
    ) -> Result<()> {
        let global = &mut ctx.accounts.global_state;
        
        if let Some(owner) = new_owner {
            global.owner = owner;
        }
        
        if let Some(staking_token_account) = new_staking_token_account {
            global.staking_token_account = staking_token_account;
        }

        Ok(())
    }

    /// Registers a new user in the matrix. If called by the user themselves, `user_key` should be their own wallet.
    /// If called by a sponsor on behalf of someone, `user_key` is the new user's wallet and the transaction payer provides funds.
    pub fn registration<'link, 'info>(
        ctx: Context<'_, '_, 'link, 'info, Registration<'info>>,
        user_key: Pubkey,
        referrer_key: Pubkey,
    ) -> Result<()>
    where
        'link: 'info,
    {
        let global = &ctx.accounts.global_state;
        require!(!global.paused, ContractError::ContractPaused);

        let payer = &ctx.accounts.payer;
        let user_account = &mut ctx.accounts.user_account;
        let referrer_acct = &mut ctx.accounts.referrer_account;
        let root_acct = &mut ctx.accounts.root_account;

        require!(
            referrer_key != Pubkey::default(),
            ContractError::InvalidReferrer
        );
        require!(
            referrer_acct.wallet == referrer_key,
            ContractError::InvalidReferrerAccount
        );
        require!(user_key != referrer_key, ContractError::InvalidReferrer);
        require!(
            user_account.matrix.is_empty(),
            ContractError::UserAlreadyExists
        );

        user_account.wallet = user_key;
        user_account.referrer = referrer_key;
        user_account.balance = 0;
        user_account.matrix = Vec::with_capacity(global.max_level as usize);
        user_account.closed_cycles = Vec::with_capacity(10);
        user_account.bump = ctx.bumps.user_account;
        user_account.matrix.push(X3 {
            blocked: false,
            referrals: 0,
            cycles: 0,
            freeze: 0,
            level_bought_time: Clock::get()?.unix_timestamp as u64,
            close_level_time: 0,
        });

        let price = global.prices[0];

        let payer_token_acct = &ctx.accounts.payer_token_account;
        let vault_token_acct = &ctx.accounts.vault_token_account;
        let staking_token_acct = &ctx.accounts.staking_token_account;
        // Ensure token accounts are correct
        require!(
            payer_token_acct.owner == payer.key(),
            ContractError::InvalidTokenAccount
        );
        require!(
            payer_token_acct.mint == global.token_mint,
            ContractError::InvalidTokenAccount
        );
        require!(
            vault_token_acct.key() == global.vault_token_account,
            ContractError::InvalidVaultAccount
        );
        // Transfer tokens from payer to vault (authority = payer who is a Signer)
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.as_ref().to_account_info(),
                Transfer {
                    from: payer_token_acct.to_account_info(),
                    to: vault_token_acct.to_account_info(),
                    authority: payer.to_account_info(),
                },
            ),
            price,
        )?;

        let mut receiver_acct = find_receiver_account(
            user_account,
            user_key,
            0,
            ctx.program_id,
            ctx.remaining_accounts,
        )?;

        let receiver_account_info = receiver_acct.to_account_info();
        let mut send_to_staking = 0;
        if receiver_acct.wallet == user_account.referrer {
            send_to_staking += update_matrix(
                &mut receiver_acct,
                receiver_account_info,
                user_key,
                0,
                global,
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            )?.unwrap_or_default();
        }

        send_to_staking += distribute_dividends(
            user_account,
            &mut receiver_acct,
            global,
            user_key,
            0,
            price,
            ctx.remaining_accounts,
        )?.unwrap_or_default();

        if send_to_staking != 0 {
            let seeds = &[b"state".as_ref(), &[global.bump]];
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.as_ref().to_account_info(),
                    Transfer {
                        from: vault_token_acct.to_account_info(),
                        to: staking_token_acct.to_account_info(),
                        authority: ctx.accounts.global_state.to_account_info(),
                    },
                    &[seeds],
                ),
                send_to_staking,
            )?;
        }


        emit!(RegistrationEvent {
            user: user_key,
            referrer: referrer_key,
            amount: price,
        });

        if root_acct.key() == receiver_acct.key() {
            ctx.accounts.root_account = receiver_acct;
        } else if ctx.accounts.referrer_account.key() == receiver_acct.key() {
            ctx.accounts.referrer_account = receiver_acct;
        } else {
            receiver_acct.exit(ctx.program_id)?;
        }

        Ok(())
    }

    /// Buy (activate) a new level for a user.
    /// If `user_key` is the caller’s own wallet, they purchase for themselves;
    /// otherwise an external payer buys a level on behalf of `user_key`.
    pub fn buy_new_level<'link, 'info>(
        ctx: Context<'_, '_, 'link, 'info, BuyNewLevel<'info>>,
        user_key: Pubkey,
        level: u8,
    ) -> Result<()>
    where
        'link: 'info,
    {
        let global = &ctx.accounts.global_state;
        require!(!global.paused, ContractError::ContractPaused);

        let payer = &ctx.accounts.payer;
        let user_account = &mut ctx.accounts.user_account;
        let root_acct = &mut ctx.accounts.root_account;
        require!(
            is_user_exists(user_account, global.root),
            ContractError::UserNotExists
        );
        require!(level < global.max_level, ContractError::LevelOutOfRange);
        require!(
            !is_level_active(user_account, level),
            ContractError::LevelAlreadyActive
        );
        require!(
            level == 0 || is_level_active(user_account, level - 1),
            ContractError::PreviousLevelRequired
        );

        if level > 0 && user_account.matrix[(level - 1) as usize].blocked {
            user_account.matrix[(level - 1) as usize].blocked = false;
        }
        user_account.matrix.push(X3 {
            blocked: false,
            referrals: 0,
            cycles: 0,
            freeze: 0,
            level_bought_time: Clock::get()?.unix_timestamp as u64,
            close_level_time: 0,
        });

        // Payment for the level
        let price = global.prices[level as usize];

        let payer_token_acct = &ctx.accounts.payer_token_account;
        let vault_token_acct = &ctx.accounts.vault_token_account;
        let staking_token_acct = &ctx.accounts.staking_token_account;
        require!(
            payer_token_acct.owner == payer.key(),
            ContractError::InvalidTokenAccount
        );
        require!(
            payer_token_acct.mint == global.token_mint,
            ContractError::InvalidTokenAccount
        );
        require!(
            vault_token_acct.key() == global.vault_token_account,
            ContractError::InvalidVaultAccount
        );
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token::Transfer {
                    from: payer_token_acct.to_account_info(),
                    to: vault_token_acct.to_account_info(),
                    authority: payer.to_account_info(),
                },
            ),
            price,
        )?;

        let mut receiver_acct = find_receiver_account(
            user_account,
            user_key,
            level,
            ctx.program_id,
            ctx.remaining_accounts,
        )?;

        let receiver_account_info = receiver_acct.to_account_info();
        let mut send_to_staking = 0;
        if receiver_acct.wallet == user_account.referrer {
            send_to_staking += update_matrix(
                &mut receiver_acct,
                receiver_account_info,
                user_key,
                level,
                global,
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            )?.unwrap_or_default();
        }

        send_to_staking += distribute_dividends(
            user_account,
            &mut receiver_acct,
            global,
            user_key,
            level,
            price,
            ctx.remaining_accounts,
        )?.unwrap_or_default();

        if send_to_staking != 0 {
            let seeds = &[b"state".as_ref(), &[global.bump]];
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.as_ref().to_account_info(),
                    Transfer {
                        from: vault_token_acct.to_account_info(),
                        to: staking_token_acct.to_account_info(),
                        authority: ctx.accounts.global_state.to_account_info(),
                    },
                    &[seeds],
                ),
                send_to_staking,
            )?;
        }

        emit!(UpgradeEvent {
            user: user_key,
            level,
            amount: price,
        });

        if root_acct.key() == receiver_acct.key() {
            ctx.accounts.root_account = receiver_acct;
        } else {
            receiver_acct.exit(ctx.program_id)?;
        }

        Ok(())
    }

    /// Claim accumulated rewards for the caller. Transfers any `balance` to the user's wallet.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let global = &ctx.accounts.global_state;
        let user_acct = &mut ctx.accounts.user_account;
        let user_wallet = &ctx.accounts.user_wallet;
        let amount = user_acct.balance;
        require!(amount > 0, ContractError::NothingToClaim);

        user_acct.balance = 0;

        // Transfer tokens from vault to user's associated token account
        let vault_acct = &ctx.accounts.vault_token_account;
        let user_token_acct = &ctx.accounts.user_token_account;
        require!(
            user_token_acct.owner == user_wallet.key(),
            ContractError::InvalidTokenAccount
        );
        require!(
            user_token_acct.mint == global.token_mint,
            ContractError::InvalidTokenAccount
        );
        require!(
            vault_acct.key() == global.vault_token_account,
            ContractError::InvalidVaultAccount
        );
        // Use global_state (PDA) as authority to sign for vault token account
        let seeds = &[b"state".as_ref(), &[global.bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.as_ref().to_account_info(),
                Transfer {
                    from: vault_acct.to_account_info(),
                    to: user_token_acct.to_account_info(),
                    authority: ctx.accounts.global_state.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;

        emit!(ClaimedEvent {
            user: user_wallet.key(),
            amount,
        });
        Ok(())
    }

    /// Pause the contract (only callable by the owner). When paused, registrations and level buys are disabled.
    pub fn pause(ctx: Context<OwnerOnly>) -> Result<()> {
        let global = &mut ctx.accounts.global_state;
        global.paused = true;
        Ok(())
    }

    /// Unpause the contract (only callable by the owner).
    pub fn unpause(ctx: Context<OwnerOnly>) -> Result<()> {
        let global = &mut ctx.accounts.global_state;
        global.paused = false;
        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(root: Pubkey, owner: Pubkey, prices: Vec<u64>)]
pub struct Initialize<'info> {
    #[account(
        init,
        seeds = [b"state"],
        bump,
        payer = initializer,
        space = 8 + GlobalState::MAX_SIZE
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        init,
        seeds = [b"user", root.as_ref()],
        bump,
        payer = initializer,
        space = 8 + UserAccount::max_size(prices.len())
    )]
    pub root_account: Account<'info, UserAccount>,

    pub token_mint: Account<'info, Mint>,
    #[account(
        init_if_needed,
        payer = initializer,
        associated_token::mint = token_mint,
        associated_token::authority = global_state,
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    #[account(
        constraint = staking_token_account.mint == token_mint.key()
    )]
    pub staking_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub initializer: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct UpdatePrices<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = global_state.bump,
        has_one = owner @ ContractError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        seeds = [b"user", global_state.root.as_ref()],
        bump = root_account.bump
    )]
    pub root_account: Account<'info, UserAccount>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdateGlobal<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = global_state.bump,
        has_one = owner @ ContractError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(user_key: Pubkey, referrer_key: Pubkey)]
pub struct Registration<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [b"state"], bump = global_state.bump)]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        init,
        seeds = [b"user", user_key.as_ref()],
        bump,
        payer = payer,
        space = 8 + UserAccount::max_size(global_state.max_level as usize)
    )]
    pub user_account: Account<'info, UserAccount>,
    #[account(mut, seeds = [b"user", referrer_key.as_ref()], bump = referrer_account.bump)]
    pub referrer_account: Account<'info, UserAccount>,
    #[account(mut, seeds = [b"user", global_state.root.as_ref()], bump = global_state.root_bump)]
    pub root_account: Account<'info, UserAccount>,
    #[account(mut,
        // Ensure this account belongs to payer and matches global_state.token_mint
        constraint = payer_token_account.owner == payer.key(),
        constraint = payer_token_account.mint == global_state.token_mint
    )]
    pub payer_token_account: Account<'info, TokenAccount>,
    #[account(mut,
        // Vault account should match global state's recorded vault
        constraint = vault_token_account.key() == global_state.vault_token_account
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    #[account(mut,
        constraint = staking_token_account.key() == global_state.staking_token_account
    )]
    pub staking_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(user_key: Pubkey, level: u8)]
pub struct BuyNewLevel<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [b"state"], bump = global_state.bump)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut, seeds = [b"user", user_key.as_ref()], bump = user_account.bump)]
    pub user_account: Account<'info, UserAccount>,
    #[account(mut, seeds = [b"user", global_state.root.as_ref()], bump = global_state.root_bump)]
    pub root_account: Account<'info, UserAccount>,
    #[account(mut,
        constraint = payer_token_account.owner == payer.key(),
        constraint = payer_token_account.mint == global_state.token_mint
    )]
    pub payer_token_account: Account<'info, TokenAccount>,
    #[account(mut,
        constraint = vault_token_account.key() == global_state.vault_token_account
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    #[account(mut,
        constraint = staking_token_account.key() == global_state.staking_token_account
    )]
    pub staking_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut, seeds = [b"state"], bump = global_state.bump)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut, seeds = [b"user", user_wallet.key().as_ref()], bump = user_account.bump,
        constraint = user_account.wallet == user_wallet.key() @ ContractError::UnauthorizedClaim
    )]
    pub user_account: Account<'info, UserAccount>,
    /// The user's wallet must sign and match the user_account.wallet field.
    pub user_wallet: Signer<'info>,
    #[account(mut,
        constraint = user_token_account.owner == user_wallet.key(),
        constraint = user_token_account.mint == global_state.token_mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(mut,
        constraint = vault_token_account.key() == global_state.vault_token_account
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OwnerOnly<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = global_state.bump,
        has_one = owner @ ContractError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    /// The contract owner
    pub owner: Signer<'info>,
}

// State: Global configuration
#[account]
pub struct GlobalState {
    pub root: Pubkey,                // Root user's wallet (public key)
    pub owner: Pubkey,               // Contract owner (admin)
    pub token_mint: Pubkey,          // Token mint for payments (if any; 0 if using SOL)
    pub paused: bool,                // Pause state of contract
    pub bump: u8,                    // Bump for global_state PDA
    pub root_bump: u8,               // Bump for root user's PDA
    pub max_level: u8,               // Number of levels configured
    pub prices: Vec<u64>,            // Price for each level (in smallest currency unit)
    pub vault_token_account: Pubkey, // PDA for vault token account
    pub staking_token_account: Pubkey, // PDA for staking token account
}

impl GlobalState {
    // Maximum space needed for GlobalState (for allocation).
    // Assuming a reasonable upper bound for max_level to avoid overly large account.
    pub const MAX_SIZE: usize = 32 + 32 + 32 + 1 + 1 + 1 + 1 +  4 + (14 * 64) + 32 + 32;
}

// State: Per-user account
#[account]
#[derive(Debug)]
pub struct UserAccount {
    pub wallet: Pubkey,               // The user's wallet address
    pub referrer: Pubkey,             // Wallet of the referrer
    pub balance: u64,                 // Withdrawable balance (rewards accumulated)
    pub matrix: Vec<X3>,              // X3 matrix data for each level active
    pub closed_cycles: Vec<NewCycle>, // Boosters for the user (max 10 initially)
    pub bump: u8,                     // PDA bump for this account
}

impl UserAccount {
    // Compute max space for a UserAccount given a number of levels (max_level).
    pub fn max_size(max_level: usize) -> usize {
        Self::max_size_with_boosters(max_level, 10)
    }
    
    // Compute max space for a UserAccount with custom booster capacity
    pub fn max_size_with_boosters(max_level: usize, booster_capacity: usize) -> usize {
        // wallet (32) + referrer (32) + balance (8) + bump (1) +
        // matrix vector: 4-byte length + each element X3 size.
        // X3 struct = 1 (bool) + 1 (u8) + 1 (u8) + 8 (u64) + 8 (u64) + 8 (u64) = 27 bytes per level.
        // boosters vector: 4-byte length + each element Booster size.
        // Booster struct = 8 (u64) + 1 (u8) = 9 bytes per booster.
        32 + 32 + 8 + 1 + 4 + (max_level * 27) + 4 + (booster_capacity * 9)
    }
}

/// X3 matrix entry for a level
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct X3 {
    pub blocked: bool, // If the user is blocked on this level (did not buy next level after cycle)
    pub referrals: u8, // Number of referrals in the current cycle (0 to 3, resets to 1 on cycle)
    pub cycles: u8,    // Number of times this level has cycled for the user
    pub freeze: u64,   // Amount currently frozen at this level for the user
    pub level_bought_time: u64,
    pub close_level_time: u64,
}

/// Booster entry for a user
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct NewCycle {
    pub created_time: u64, // Unix timestamp when boost was created
    pub level: u8,         // Level of the boost
}

#[event]
pub struct RegistrationEvent {
    pub user: Pubkey,
    pub referrer: Pubkey,
    pub amount: u64,
}
#[event]
pub struct UpgradeEvent {
    pub user: Pubkey,
    pub level: u8,
    pub amount: u64,
}
#[event]
pub struct MissedReceiveEvent {
    pub receiver: Pubkey,
    pub from: Pubkey,
    pub level: u8,
    pub mode: u8, // 0 = overtook (referrer not active), 1 = blocked
}
#[event]
pub struct SentDividendsEvent {
    pub receiver: Pubkey,
    pub from: Pubkey,
    pub level: u8,
    pub amount: u64,
    pub mode: u8, // 0 = simple (direct), 1 = extra (overtaken)
}
#[event]
pub struct NewUserPlaceEvent {
    pub user: Pubkey,
    pub caller: Pubkey,
    pub level: u8,
    pub place: u8,
}
#[event]
pub struct ReinvestEvent {
    pub user: Pubkey,
    pub caller: Pubkey,
    pub level: u8,
    pub count: u8,
}
#[event]
pub struct ClaimedEvent {
    pub user: Pubkey,
    pub amount: u64,
}
#[event]
pub struct FrozenEvent {
    pub user: Pubkey,
    pub level: u8,
    pub amount: u64,
}
#[event]
pub struct UnfrozenEvent {
    pub user: Pubkey,
    pub level: u8,
    pub amount: u64,
}
#[event]
pub struct BurnedEvent {
    pub amount: u64,
}
#[event]
pub struct BoosterCreatedEvent {
    pub user: Pubkey,
    pub level: u8,
    pub cycles: u8,
    pub created_time: u64,
}
#[event]
pub struct AccountResizedEvent {
    pub user: Pubkey,
    pub old_size: u32,
    pub new_size: u32,
}

fn is_user_exists(user_account: &UserAccount, root_wallet: Pubkey) -> bool {
    // A user exists if their referrer is non-zero or if they are the root user
    user_account.referrer != Pubkey::default() || user_account.wallet == root_wallet
}
fn is_level_active(user_account: &UserAccount, level: u8) -> bool {
    // Level is active if the user_account has a matrix entry for that index
    (user_account.matrix.len() as u8) > level
}

fn update_matrix<'info, 'link>(
    receiver_acct: &mut UserAccount,
    receiver_account_info: AccountInfo<'info>,
    caller: Pubkey,
    level: u8,
    global: &GlobalState,
    user_wallet: AccountInfo<'info>,
    system_program: AccountInfo<'info>,
) -> Result<Option<u64>>
where
    'link: 'info,
{
    let lvl = level as usize;
    receiver_acct.matrix[lvl].referrals += 1;
    let place = receiver_acct.matrix[lvl].referrals;
    emit!(NewUserPlaceEvent {
        user: receiver_acct.wallet,
        caller,
        level,
        place,
    });
    if receiver_acct.matrix[lvl].referrals >= 3 {
        receiver_acct.matrix[lvl].referrals = 0;
        receiver_acct.matrix[lvl].cycles += 1;

        if receiver_acct.matrix[lvl].cycles == 1 {
            receiver_acct.matrix[lvl].close_level_time = Clock::get()?.unix_timestamp as u64;
        } else {
            // Add booster when cycles > 1
            let booster = NewCycle {
                created_time: Clock::get()?.unix_timestamp as u64,
                level,
            };

            if receiver_acct.closed_cycles.len() >= receiver_acct.closed_cycles.capacity() {
                resize_user_account_for_boosters(
                    receiver_acct,
                    receiver_account_info,
                    user_wallet,
                    system_program,
                    global,
                    receiver_acct.closed_cycles.capacity() * 2,
                )?;
            }

            // Add the booster
            receiver_acct.closed_cycles.push(booster);
            
            emit!(BoosterCreatedEvent {
                user: receiver_acct.wallet,
                level,
                cycles: receiver_acct.matrix[lvl].cycles,
                created_time: Clock::get()?.unix_timestamp as u64
            });
        }
        
        emit!(ReinvestEvent {
            user: receiver_acct.wallet,
            caller,
            level,
            count: receiver_acct.matrix[lvl].cycles,
        });
        if level < global.max_level - 1 && !is_level_active(receiver_acct, level + 1) {
            receiver_acct.matrix[lvl].blocked = true;
        }
        return Ok(unfreeze(receiver_acct, level))
    }

    Ok(None)
}

fn distribute_dividends(
    user_account: &mut UserAccount,
    receiver_acct: &mut UserAccount,
    global: &GlobalState,
    user_key: Pubkey,
    level: u8,
    price: u64,
    remaining_accounts: &[AccountInfo],
) -> Result<Option<u64>> {
    let mut return_value: u64 = 0;
    if user_account.referrer != receiver_acct.wallet {
        let extra = price / 2;

        // Only add to balance if receiver is not root
        if receiver_acct.wallet != global.root {
            receiver_acct.balance = receiver_acct.balance.checked_add(extra).unwrap();
            return_value += extra;
        } else {
            // If receiver is root, return all dividends (base + extra)
            return_value += price;
        }

        emit!(SentDividendsEvent {
            receiver: receiver_acct.wallet,
            from: user_key,
            level,
            amount: return_value,
            mode: 1, // extra
        });
        return Ok(Some(return_value));
    }

    let lvl = level as usize;
    let recv_ref_count = receiver_acct.matrix[lvl].referrals;
    let mut dividends: u64 = 0;

    if recv_ref_count == 1 || recv_ref_count == 2{
        dividends = price / 2;
    } else if recv_ref_count == 0 {
        dividends = price;
        let mut current_wallet = receiver_acct.referrer;
        if current_wallet == Pubkey::default() {
            current_wallet = global.root;
        }

        loop {
            if current_wallet == global.root {
                return_value += dividends;
                break;
            }
            
            let current_info = remaining_accounts.iter().find(|info| {
                info.key
                    == &Pubkey::find_program_address(
                        &[b"user", current_wallet.as_ref()],
                        &ID,
                    )
                    .0
            }).ok_or(ContractError::MissingUplineAccount)?;

            let mut current_user = {
                let mut data: &[u8] = &current_info.try_borrow_data()?;
                UserAccount::try_deserialize(&mut data)?
            };
            
            if is_level_active(&current_user, level) {
                current_user.balance = current_user.balance.checked_add(dividends).unwrap();
                let mut dst = current_info.try_borrow_mut_data()?;
                current_user.try_serialize(&mut *dst)?;

                emit!(SentDividendsEvent {
                    receiver: current_user.wallet,
                    from: user_key,
                    level,
                    amount: dividends,
                    mode: 1, // extra
                });
                break;
            }

            current_wallet = current_user.referrer;
            if current_wallet == Pubkey::default() {
                current_wallet = global.root;
            }
        }
    }

    user_account.matrix[lvl].freeze = price;
    emit!(FrozenEvent {
        user: user_key,
        level,
        amount: price,
    });

    // Only add to freeze if receiver is not root
    if receiver_acct.wallet != global.root {
        receiver_acct.matrix[lvl].freeze = receiver_acct.matrix[lvl]
            .freeze
            .checked_add(dividends)
            .unwrap();
    } else {
        // If receiver is root, return all dividends
        return_value += dividends;
    }

    emit!(SentDividendsEvent {
        receiver: receiver_acct.wallet,
        from: user_key,
        level,
        amount: dividends,
        mode: 0, // simple (direct)
    });


    if return_value > 0 {
        Ok(Some(return_value))
    } else {
        Ok(None)
    }
}

fn unfreeze(user: &mut UserAccount, level: u8) -> Option<u64> {
    let lvl = level as usize;
    let freeze_amount = user.matrix[lvl].freeze;
    if freeze_amount > 0 {
        user.matrix[lvl].freeze = 0;
        let burn = freeze_amount / 200; // 0.5% burn
        let release = freeze_amount - burn;
        user.balance = user.balance.checked_add(release).unwrap();
        emit!(UnfrozenEvent {
            user: user.wallet,
            level,
            amount: release,
        });
        emit!(BurnedEvent { amount: burn });
        return Some(burn);
    }
    None
}

fn find_receiver_account<'info, 'link>(
    user_account: &Account<UserAccount>,
    user_key: Pubkey,
    level: u8,
    ctx_program_id: &Pubkey,
    remaining_accounts: &'link [AccountInfo<'info>],
) -> Result<Account<'info, UserAccount>>
where
    'link: 'info,
{
    let next_upline_wallet = user_account.referrer;
    let next_acc_info = remaining_accounts
        .iter()
        .find(|info| {
            info.key
                == &Pubkey::find_program_address(
                    &[b"user", next_upline_wallet.as_ref()],
                    ctx_program_id,
                )
                .0
        })
        .ok_or(ContractError::MissingUplineAccount)?;

    let next_user = Account::try_from(next_acc_info)?;

    let mode = if !is_level_active(&next_user, level) {
        0
    } else if next_user
        .matrix
        .get(level as usize)
        .map(|l| l.blocked)
        .unwrap_or_default()
    {
        1
    } else {
        return Ok(next_user);
    };

    emit!(MissedReceiveEvent {
        receiver: next_user.wallet,
        from: user_key,
        level,
        mode,
    });
    find_receiver_account(
        &next_user,
        user_key,
        level,
        ctx_program_id,
        remaining_accounts,
    )
}

fn resize_user_account_for_boosters<'info>(
    user_account: &mut UserAccount,
    user_account_info: AccountInfo<'info>,
    user_wallet: AccountInfo<'info>,
    system_program: AccountInfo<'info>,
    global_state: &GlobalState,
    new_booster_capacity: usize,
) -> Result<()> {
    let current_size = user_account_info.data_len();
    let old_capacity = user_account.closed_cycles.capacity() as u32;

    let new_size = UserAccount::max_size_with_boosters(
        global_state.max_level as usize,
        new_booster_capacity,
    );

    // Only resize if the new size is larger
    if new_size > current_size {
        // Calculate rent for the new size
        let rent = Rent::get()?;
        let new_rent = rent.minimum_balance(new_size);
        let current_rent = rent.minimum_balance(current_size);
        let additional_rent = new_rent - current_rent;

        // Transfer additional rent from user to the account
        if additional_rent > 0 {
            system_program::transfer(
                CpiContext::new(
                    system_program.clone(),
                    system_program::Transfer {
                        from: user_wallet.clone(),
                        to: user_account_info.clone(),
                    },
                ),
                additional_rent,
            )?;
        }

        // Resize the account using the modern method
        user_account_info.resize(new_size)?;
    }

    // Update the boosters vector capacity
    user_account.closed_cycles.reserve(new_booster_capacity);

    emit!(AccountResizedEvent {
        user: user_account.wallet,
        old_size: old_capacity,
        new_size: new_booster_capacity as u32,
    });

    Ok(())
}

#[error_code]
pub enum ContractError {
    #[msg("The contract is paused")]
    ContractPaused,
    #[msg("Referrer address is invalid")]
    InvalidReferrer,
    #[msg("Referrer account provided does not match referrer")]
    InvalidReferrerAccount,
    #[msg("User already exists")]
    UserAlreadyExists,
    #[msg("User does not exist")]
    UserNotExists,
    #[msg("Level out of range")]
    LevelOutOfRange,
    #[msg("This level is already activated for the user")]
    LevelAlreadyActive,
    #[msg("Previous level must be activated first")]
    PreviousLevelRequired,
    #[msg("Unauthorized: only the contract owner can call this")]
    Unauthorized,
    #[msg("Unauthorized: cannot claim for another user")]
    UnauthorizedClaim,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("Invalid token account provided")]
    InvalidTokenAccount,
    #[msg("Invalid or mismatched vault account")]
    InvalidVaultAccount,
    #[msg("Required upline account not provided")]
    MissingUplineAccount,
    #[msg("Provided account is not writable")]
    AccountNotWritable,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Invalid resize capacity - must be greater than current capacity")]
    InvalidResizeCapacity,
    #[msg("Excessive booster capacity - maximum 100 boosters allowed")]
    ExcessiveBoosterCapacity,
}

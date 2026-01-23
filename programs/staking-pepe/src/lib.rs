#![allow(deprecated)]
use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use x3_pepe::{
    self,
    UserAccount,
};

declare_id!("9Mq2JHE2c38LTioLQYjqbLDd4jJkgftLj83VYiTYrfnJ");

pub use x3_pepe::ID as X3_PROGRAM_ID;

// Seconds in a (365d) year, used for APR -> per-second rate.
const SECONDS_PER_YEAR: i128 = 31_536_000;
// Max supported staking levels. Must match allocated space assumptions.
const MAX_LEVELS: usize = 14;

#[program]
pub mod staking_pepe {
    use super::*;

    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        _pool_bump: u8,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.authority.key();
        pool.staking_mint = ctx.accounts.staking_mint.key();
        pool.total_staked = 0;
        pool.bump = ctx.bumps.pool;

        emit!(InitializePoolEvent {
            authority: pool.authority,
            staking_mint: pool.staking_mint,
            pool: pool.key(),
        });

        Ok(())
    }

    pub fn deposit_rewards(ctx: Context<DepositRewards>, amount: u64) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.pool.authority,
            ctx.accounts.authority.key(),
            StakingError::Unauthorized
        );

        let cpi_accounts = Transfer {
            from: ctx.accounts.from_authority_ata.to_account_info(),
            to: ctx.accounts.stake_vault.to_account_info(),
            authority: ctx.accounts.authority.to_account_info(),
        };
        let cpi = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi, amount)?;

        emit!(DepositRewardsEvent {
            authority: ctx.accounts.authority.key(),
            pool:  ctx.accounts.pool.key(),
            amount,
        });

        Ok(())
    }

    pub fn init_user(ctx: Context<InitUser>) -> Result<()> {
        let user = &mut ctx.accounts.user_stake;
        user.pool = ctx.accounts.pool.key();
        user.owner = ctx.accounts.owner.key();
        user.staking_by_level = Vec::new();

        emit!(InitUserEvent {
            owner: user.owner,
            pool: user.pool,
        });

        Ok(())
    }

    pub fn update_limits(ctx: Context<UpdateLimits>, staking_limits_by_level: Vec<StakingLimit>) -> Result<()> {
        require!(
            staking_limits_by_level.len() <= MAX_LEVELS,
            StakingError::TooManyLevels
        );

        for lvl in staking_limits_by_level.iter() {
            require!(lvl.min <= lvl.max, StakingError::InvalidLimits);
        }

        ctx.accounts.pool.staking_limits_by_level = staking_limits_by_level.clone();

        emit!(UpdateLimitsEvent {
            authority: ctx.accounts.authority.key(),
            levels_count: staking_limits_by_level.len() as u64,
            pool: ctx.accounts.pool.key(),
        });

        Ok(())
    }

    /// Updates the pool authority.
    /// Only the current authority can call this function.
    pub fn update_authority(ctx: Context<UpdateAuthority>, new_authority: Pubkey) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let old_authority = pool.authority;
        pool.authority = new_authority;

        emit!(UpdateAuthorityEvent {
            old_authority,
            new_authority,
            pool: pool.key(),
        });

        Ok(())
    }

    pub fn stake(
        ctx: Context<Stake>,
        amount: u64,
        level: u8
    ) -> Result<()> {
        require!(
            level < ctx.accounts.pool.staking_limits_by_level.len() as u8,
            StakingError::LevelOutOfRange
        );

        let level_limits = &ctx.accounts.pool.staking_limits_by_level[level as usize];
        let level_idx = level as usize;
        let current_time = Clock::get()?.unix_timestamp as u64;
        
        // Ensure staking_by_level vector is large enough
        while ctx.accounts.user_stake.staking_by_level.len() <= level_idx {
            ctx.accounts.user_stake.staking_by_level.push(StakingInfo {
                amount_staked: 0,
                rewards_accrued: 0,
                last_update_ts: 0,
                start_staking_time: 0,
                boost_rewards_total: 0,
                cycle_used: 0,
            });
        }

        let already_stake = ctx.accounts.user_stake.staking_by_level[level_idx].amount_staked;
        let is_new_stake = already_stake == 0;
        let used_cycles = ctx.accounts.user_stake.staking_by_level[level_idx].cycle_used;
        let is_restake = is_new_stake && used_cycles > 0;

        if is_new_stake {
            // New stake or restake: requires cycle and must meet min limit
            let cycles = get_x3_cycles(
                &ctx.accounts.external_state,
                ctx.accounts.user_stake.owner,
                level,
            )?;

            require!(
                amount >= level_limits.min && amount <= level_limits.max,
                StakingError::AmountOutOfLimits
            );

            require!(
                (cycles as u64) > used_cycles,
                StakingError::NoStakingRights
            );
            ctx.accounts.user_stake.staking_by_level[level_idx].cycle_used = used_cycles
                .checked_add(1)
                .ok_or(StakingError::MathOverflow)?;
        } else {
            // Adding to existing stake: check period hasn't ended and new total doesn't exceed max
            let start_time = ctx.accounts.user_stake.staking_by_level[level_idx].start_staking_time;
            let period_end_time = start_time
                .checked_add(level_limits.period)
                .ok_or(StakingError::MathOverflow)?;
            
            require!(
                current_time < period_end_time,
                StakingError::StakingPeriodEnded
            );

            let new_total = already_stake
                .checked_add(amount)
                .ok_or(StakingError::MathOverflow)?;
            require!(
                new_total <= level_limits.max,
                StakingError::AmountOutOfLimits
            );
        }

        if !is_new_stake {
            let accrued = accrue_rewards_internal(
                &ctx.accounts.pool,
                &mut ctx.accounts.user_stake,
                &ctx.accounts.external_state,
            )?;

            // Emit accrual events
            for reward in accrued {
                emit!(AccrueRewardsEvent {
                owner: ctx.accounts.owner.key(),
                pool: ctx.accounts.pool.key(),
                level: reward.level,
                amount: reward.accrued_amount,
                total_rewards: reward.total_rewards,
                boost_rewards: reward.boost_rewards,
                total_boost_rewards: reward.total_boost_rewards,
            });
            }
        }

        let cpi_accounts = Transfer {
            from: ctx.accounts.from_user_ata.to_account_info(),
            to: ctx.accounts.stake_vault.to_account_info(),
            authority: ctx.accounts.owner.to_account_info(),
        };
        let cpi = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi, amount)?;

        if is_new_stake {
            ctx.accounts.user_stake.staking_by_level[level_idx].start_staking_time = current_time;
        }
        ctx.accounts.user_stake.staking_by_level[level_idx].amount_staked = ctx.accounts.user_stake.staking_by_level[level_idx]
            .amount_staked
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;
        ctx.accounts.user_stake.staking_by_level[level_idx].last_update_ts = current_time;

        ctx.accounts.pool.total_staked = ctx.accounts.pool
            .total_staked
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;

        // Emit appropriate event based on staking type
        if is_restake {
            emit!(RestakeEvent {
                owner: ctx.accounts.owner.key(),
                pool: ctx.accounts.pool.key(),
                level,
                amount,
            });
        } else if is_new_stake {
            emit!(NewStakeEvent {
                owner: ctx.accounts.owner.key(),
                pool: ctx.accounts.pool.key(),
                level,
                amount,
            });
        } else {
            let total_amount = ctx.accounts.user_stake.staking_by_level[level_idx].amount_staked;
            emit!(AddToStakeEvent {
                owner: ctx.accounts.owner.key(),
                pool: ctx.accounts.pool.key(),
                level,
                amount,
                total_amount,
            });
        }

        Ok(())
    }


    pub fn unstake(ctx: Context<Unstake>, level: u8) -> Result<()> {
        require!(
            level < ctx.accounts.user_stake.staking_by_level.len() as u8,
            StakingError::LevelOutOfRange
        );

        let level_idx = level as usize;
        let level_staking = &ctx.accounts.user_stake.staking_by_level[level_idx];
        let amount_staked = level_staking.amount_staked;
        require!(amount_staked > 0, StakingError::NothingToUnstake);

        // Check that staking period has ended before allowing unstake
        require!(
            level < ctx.accounts.pool.staking_limits_by_level.len() as u8,
            StakingError::LevelOutOfRange
        );
        let level_limits = &ctx.accounts.pool.staking_limits_by_level[level_idx];
        let current_time = Clock::get()?.unix_timestamp as u64;
        let start_staking_time = level_staking.start_staking_time;
        let period_end_time = start_staking_time
            .checked_add(level_limits.period)
            .ok_or(StakingError::MathOverflow)?;
        
        // Require that start_staking_time + period <= current_time (period has ended)
        require!(
            period_end_time <= current_time,
            StakingError::StakingPeriodNotEnded
        );

        let accrued = accrue_rewards_internal(
            &ctx.accounts.pool,
            &mut ctx.accounts.user_stake,
            &ctx.accounts.external_state,
        )?;

        // Emit accrual events
        for reward in accrued {
            emit!(AccrueRewardsEvent {
                owner: ctx.accounts.user_stake.owner,
                pool: ctx.accounts.pool.key(),
                level: reward.level,
                amount: reward.accrued_amount,
                total_rewards: reward.total_rewards,
                boost_rewards: reward.boost_rewards,
                total_boost_rewards: reward.total_boost_rewards,
            });
        }

        let level_staking = &mut ctx.accounts.user_stake.staking_by_level[level_idx];
        let rewards = level_staking.rewards_accrued;

        let seeds = &[
            b"pool",
            ctx.accounts.pool.authority.as_ref(),
            ctx.accounts.pool.staking_mint.as_ref(),
            &[ctx.accounts.pool.bump],
        ];
        let signer = &[&seeds[..]];

        let total_amount = amount_staked
            .checked_add(rewards)
            .ok_or(StakingError::MathOverflow)?;
        
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.stake_vault.to_account_info(),
                    to: ctx.accounts.to_user_stake_ata.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                signer,
            ),
            total_amount,
        )?;

        ctx.accounts.pool.total_staked = ctx.accounts.pool.total_staked
            .checked_sub(amount_staked)
            .ok_or(StakingError::MathOverflow)?;

        level_staking.amount_staked = 0;
        level_staking.rewards_accrued = 0;
        level_staking.start_staking_time = 0;
        level_staking.last_update_ts = Clock::get()?.unix_timestamp as u64;

        emit!(UnstakeEvent {
            owner: ctx.accounts.user_stake.owner,
            pool: ctx.accounts.pool.key(),
            level,
            amount: amount_staked,
            rewards,
        });

        Ok(())
    }

    pub fn update_user_rewards(ctx: Context<UpdateUserRewards>) -> Result<()> {
        let accrued = accrue_rewards_internal(
            &ctx.accounts.pool,
            &mut ctx.accounts.user_stake,
            &ctx.accounts.external_state,
        )?;

        // Emit accrual events
        for reward in accrued {
            emit!(AccrueRewardsEvent {
                owner: ctx.accounts.user_stake.owner,
                pool: ctx.accounts.pool.key(),
                level: reward.level,
                amount: reward.accrued_amount,
                total_rewards: reward.total_rewards,
                boost_rewards: reward.boost_rewards,
                total_boost_rewards: reward.total_boost_rewards,
            });
        }

        Ok(())
    }
}

fn accrue_rewards_internal(
    pool: &Account<Pool>,
    user: &mut Account<UserStake>,
    external_account: &Account<UserAccount>,
) -> Result<Vec<AccruedReward>> {
    // Returns Vec<AccruedReward> with reward information for each level
    // Ensure wallet == user.owner
    require_keys_eq!(external_account.wallet, user.owner, StakingError::InvalidExternalStateData);

    // Convert boosters (closed_cycles) to Vec<(u64, u8)> format
    let boosters: Vec<(u64, u8)> = external_account.closed_cycles
        .iter()
        .map(|cycle| (cycle.created_time, cycle.level))
        .collect();

    let now = Clock::get()?.unix_timestamp as u64;
    let mut accrued_rewards: Vec<AccruedReward> = Vec::new();

    // Process each level staking
    for (level, level_staking) in user.staking_by_level.iter_mut().enumerate() {
        // Check if staking period has ended (start_staking_time + period > now)
        let level_limits = if level < pool.staking_limits_by_level.len() {
            &pool.staking_limits_by_level[level]
        } else {
            continue; // Skip if no limits defined for this level
        };

        if level_staking.amount_staked == 0 {
            continue;
        }

        let period_end_time = level_staking
            .start_staking_time
            .checked_add(level_limits.period)
            .ok_or(StakingError::MathOverflow)?;
        let reward_start_time = level_staking.last_update_ts;

        if period_end_time <= reward_start_time {
            continue; // Period has ended, no more rewards
        }

        // Calculate time delta from max(last_update_ts, close_level_time)
        if now <= reward_start_time {
            continue;
        }

        let reward_end_time = now.min(period_end_time);

        let dt: i128 = (reward_end_time as i128) - (reward_start_time as i128);
        
        // Use APY from stake limits for this level
        let apy_bps = level_limits.apy_bps as i128;

        // Rewards = stake * (apr_bps/10_000) * (dt/seconds_per_year)
        let stake: i128 = level_staking.amount_staked as i128;
        let numer = stake
            .checked_mul(apy_bps)
            .ok_or(StakingError::MathOverflow)?
            .checked_mul(dt)
            .ok_or(StakingError::MathOverflow)?;
        let reward_i128 = numer / (10_000 * SECONDS_PER_YEAR);

        let mut level_accrued: u64 = 0;
        let mut level_boost_accrued: u64 = 0;
        
        if reward_i128 > 0 {
            let reward_u64: u64 = u64::try_from(reward_i128).map_err(|_| StakingError::MathOverflow)?;
            level_accrued = reward_u64;
            level_staking.rewards_accrued = level_staking
                .rewards_accrued
                .checked_add(reward_u64)
                .ok_or(StakingError::MathOverflow)?;
        }

        for boost in boosters.iter().filter(|boost| boost.1 as usize == level) {
            // Compute boost interval within [reward_start_time, reward_end_time]
            let boost_period_end = boost
                .0
                .checked_add(level_limits.boost_by_cycle.period)
                .unwrap_or(u64::MAX);
            let boost_end = reward_end_time.min(boost_period_end);
            let boost_start = boost.0.max(reward_start_time);
            if boost_end > boost_start  {
                // Additional reward using boost APY only for the boost interval.
                let boost_apy_bps = level_limits.boost_by_cycle.apy as i128;
                let dt = boost_end as i128 - boost_start as i128;
                let numer_boost = stake
                    .checked_mul(boost_apy_bps)
                    .ok_or(StakingError::MathOverflow)?
                    .checked_mul(dt)
                    .ok_or(StakingError::MathOverflow)?;
                let reward_boost_i128 = numer_boost / (10_000 * SECONDS_PER_YEAR);
                if reward_boost_i128 > 0 {
                    let reward_u64: u64 = u64::try_from(reward_boost_i128)
                        .map_err(|_| StakingError::MathOverflow)?;
                    level_boost_accrued = level_boost_accrued
                        .checked_add(reward_u64)
                        .ok_or(StakingError::MathOverflow)?;
                    level_accrued = level_accrued
                        .checked_add(reward_u64)
                        .ok_or(StakingError::MathOverflow)?;
                    level_staking.rewards_accrued = level_staking
                        .rewards_accrued
                        .checked_add(reward_u64)
                        .ok_or(StakingError::MathOverflow)?;
                    level_staking.boost_rewards_total = level_staking
                        .boost_rewards_total
                        .checked_add(reward_u64)
                        .ok_or(StakingError::MathOverflow)?;
                }
            }
        }

        level_staking.last_update_ts = now;
        
        // Track accrued rewards for this level (including boost rewards)
        if level_accrued > 0 {
            accrued_rewards.push(AccruedReward {
                level: level as u8,
                accrued_amount: level_accrued,
                total_rewards: level_staking.rewards_accrued,
                boost_rewards: level_boost_accrued,
                total_boost_rewards: level_staking.boost_rewards_total,
            });
        }
    }

    Ok(accrued_rewards)
}

fn get_x3_cycles(
    external_account: &Account<UserAccount>,
    expected_owner: Pubkey,
    required_level: u8,
) -> Result<u8> {
    require_keys_eq!(external_account.wallet, expected_owner, StakingError::InvalidExternalStateData);

    require!(
        external_account.matrix.len() > required_level as usize,
        StakingError::InsufficientX3Level
    );

    Ok(external_account.matrix[required_level as usize].cycles)
}

#[derive(Clone, AnchorDeserialize, AnchorSerialize)]
pub struct StakingLimit {
    pub min: u64,
    pub max: u64,
    pub period: u64,
    pub apy_bps: u64,
    pub boost_by_cycle: Boost
}

#[account]
pub struct Pool {
    pub authority: Pubkey,
    pub staking_mint: Pubkey,
    pub total_staked: u64,
    pub staking_limits_by_level: Vec<StakingLimit>,
    pub bump: u8,
}
impl Pool {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 1 + 8 + MAX_LEVELS * (8 + 8 + 8 + 8 + 8 + 8 + 8);
}

#[derive(Clone, AnchorDeserialize, AnchorSerialize)]
pub struct StakingInfo {
    pub amount_staked: u64,
    pub rewards_accrued: u64,
    pub last_update_ts: u64,
    pub start_staking_time: u64,
    pub boost_rewards_total: u64,
    pub cycle_used: u64,
}

#[derive(Clone, AnchorDeserialize, AnchorSerialize)]
pub struct Boost {
    pub apy: u64,
    pub period: u64,
}

#[derive(Clone, Debug)]
pub struct AccruedReward {
    pub level: u8,
    pub accrued_amount: u64,
    pub total_rewards: u64,
    pub boost_rewards: u64,
    pub total_boost_rewards: u64,
}

#[account]
pub struct UserStake {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub staking_by_level: Vec<StakingInfo>,
}
impl UserStake {
    pub const LEN: usize =
        8 + 32 + 32 + (8 + 8 + 8 + 8 + 8 + 8) * MAX_LEVELS;
}

#[derive(Accounts)]
#[instruction(_pool_bump: u8)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    pub staking_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = authority,
        space = Pool::LEN,
        seeds = [b"pool", authority.key().as_ref(), staking_mint.key().as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init_if_needed,
        payer = authority,
        associated_token::mint = staking_mint,
        associated_token::authority = pool,
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct DepositRewards<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority,
        seeds = [b"pool", pool.authority.as_ref(), pool.staking_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        associated_token::mint = pool.staking_mint,
        associated_token::authority = authority
    )]
    pub from_authority_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        associated_token::mint = pool.staking_mint,
        associated_token::authority = pool
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct UpdateLimits<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority,
        seeds = [b"pool", pool.authority.as_ref(), pool.staking_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct UpdateAuthority<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ StakingError::Unauthorized
    )]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct InitUser<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        seeds = [b"pool", pool.authority.as_ref(), pool.staking_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init,
        payer = owner,
        space = UserStake::LEN,
        seeds = [b"user", pool.key().as_ref(), owner.key().as_ref()],
        bump
    )]
    pub user_stake: Account<'info, UserStake>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [b"pool", pool.authority.as_ref(), pool.staking_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"user", pool.key().as_ref(), owner.key().as_ref()],
        bump
    )]
    pub user_stake: Account<'info, UserStake>,

    #[account(
        mut,
        associated_token::mint = pool.staking_mint,
        associated_token::authority = owner
    )]
    pub from_user_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        associated_token::mint = pool.staking_mint,
        associated_token::authority = pool
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    #[account(
        owner = x3_pepe::ID @ StakingError::InvalidExternalStateOwner
    )]
    pub external_state: Account<'info, UserAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [b"pool", pool.authority.as_ref(), pool.staking_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"user", pool.key().as_ref(), user_stake.owner.as_ref()],
        bump,
        has_one = owner
    )]
    pub user_stake: Account<'info, UserStake>,

    #[account(
        mut,
        associated_token::mint = pool.staking_mint,
        associated_token::authority = pool
    )]
    pub stake_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        associated_token::mint = pool.staking_mint,
        associated_token::authority = owner
    )]
    pub to_user_stake_ata: Account<'info, TokenAccount>,

    #[account(
        owner = x3_pepe::ID @ StakingError::InvalidExternalStateOwner
    )]
    pub external_state: Account<'info, UserAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct UpdateUserRewards<'info> {
    #[account(
        seeds = [b"pool", pool.authority.as_ref(), pool.staking_mint.as_ref()],
        bump = pool.bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"user", pool.key().as_ref(), user_stake.owner.as_ref()],
        bump
    )]
    pub user_stake: Account<'info, UserStake>,

    #[account(
        owner = x3_pepe::ID @ StakingError::InvalidExternalStateOwner
    )]
    pub external_state: Account<'info, UserAccount>,
}

#[error_code]
pub enum StakingError {
    #[msg("Unauthorized")] 
    Unauthorized,
    #[msg("Math overflow")] 
    MathOverflow,
    #[msg("Too many staking levels")] 
    TooManyLevels,
    #[msg("Invalid staking limits")] 
    InvalidLimits,
    #[msg("Invalid external state account owner")] 
    InvalidExternalStateOwner,
    #[msg("Invalid external state data")] 
    InvalidExternalStateData,
    #[msg("Level out of range")] 
    LevelOutOfRange,
    #[msg("Amount out of limits for this level")] 
    AmountOutOfLimits,
    #[msg("Insufficient X3 level")] 
    InsufficientX3Level,
    #[msg("No available staking rights for this level")]
    NoStakingRights,
    #[msg("Staking period has ended, unstake first")]
    StakingPeriodEnded,
    #[msg("Cannot unstake before staking period ends")]
    StakingPeriodNotEnded,
    #[msg("Nothing to unstake for this level")]
    NothingToUnstake,
}



#[event]
pub struct InitializePoolEvent {
    pub authority: Pubkey,
    pub staking_mint: Pubkey,
    pub pool: Pubkey,
}

#[event]
pub struct DepositRewardsEvent {
    pub authority: Pubkey,
    pub amount: u64,
    pub pool: Pubkey,
}

#[event]
pub struct InitUserEvent {
    pub owner: Pubkey,
    pub pool: Pubkey,
}

#[event]
pub struct UpdateLimitsEvent {
    pub authority: Pubkey,
    pub levels_count: u64,
    pub pool: Pubkey,
}

#[event]
pub struct UpdateAuthorityEvent {
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
    pub pool: Pubkey,
}

#[event]
pub struct NewStakeEvent {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub level: u8,
    pub amount: u64,
}

#[event]
pub struct AddToStakeEvent {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub level: u8,
    pub amount: u64,
    pub total_amount: u64,
}

#[event]
pub struct RestakeEvent {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub level: u8,
    pub amount: u64,
}

#[event]
pub struct AccrueRewardsEvent {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub level: u8,
    pub amount: u64,
    pub total_rewards: u64,
    pub boost_rewards: u64,
    pub total_boost_rewards: u64,
}

#[event]
pub struct ClaimRewardsEvent {
    pub owner: Pubkey,
    pub level: u8,
    pub amount: u64,
    pub pool: Pubkey,
}

#[event]
pub struct UnstakeEvent {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub level: u8,
    pub amount: u64,
    pub rewards: u64,
}

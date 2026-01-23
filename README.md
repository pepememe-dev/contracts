# X3 pepe (Anchor Program)

This is a [Solana](https://solana.com/) smart contract written using the [Anchor](https://book.anchor-lang.com/) framework.

The program implements a registration-based user system using PDAs (Program Derived Addresses) for `GlobalState`, `UserAccount`. It supports initialization, user registration with referrers, and SPL token handling via associated token accounts.

---

## Project Structure

- **`programs/x3-pepe/src/lib.rs`** – Main program logic (Rust)
- **`tests/`** – Mocha-based test suite (TypeScript)
- **`Anchor.toml`** – Anchor project config
- **`migrations/`** – Deployment scripts (if any)
- **`target/idl/`** – Auto-generated IDL after build

---

## Build the Program

Compile the Solana smart contract to ensure it builds correctly.

```bash
anchor build
```

This will compile the program to `target/deploy/x3_pepe.so` and generate the IDL.

---

## Deploy the Program

Deploy to **localnet**:

```bash
anchor deploy
```

To deploy to **devnet**:

```bash
anchor deploy --provider.cluster devnet
```

Ensure you have SOL in your wallet and are connected to the right cluster (`anchor provider set --cluster devnet`).

---

## Run Tests

Run the full test suite:

```bash
anchor test
```

This will:

1. Start a local validator
2. Deploy the program
3. Run Mocha tests from the `tests/` directory


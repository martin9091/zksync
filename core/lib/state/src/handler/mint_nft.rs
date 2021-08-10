use num::{BigUint, ToPrimitive, Zero};
use std::time::Instant;

use zksync_types::{
    operations::MintNFTOp,
    tokens::NFT,
    tx::{calculate_token_address, calculate_token_data, calculate_token_hash},
    Account, AccountUpdate, AccountUpdates, Address, MintNFT, Nonce, PubKeyHash, TokenId, ZkSyncOp,
};

use zksync_crypto::params::{
    max_processable_token, MIN_NFT_TOKEN_ID, NFT_STORAGE_ACCOUNT_ADDRESS, NFT_STORAGE_ACCOUNT_ID,
    NFT_TOKEN_ID,
};

use crate::{
    handler::{error::MintNFTOpError, TxHandler},
    state::{CollectedFee, OpSuccess, ZkSyncState},
};

impl TxHandler<MintNFT> for ZkSyncState {
    type Op = MintNFTOp;
    type OpError = MintNFTOpError;

    fn create_op(&self, tx: MintNFT) -> Result<Self::Op, Self::OpError> {
        invariant!(
            tx.fee_token <= max_processable_token(),
            MintNFTOpError::InvalidTokenId
        );
        invariant!(
            tx.recipient != Address::zero(),
            MintNFTOpError::RecipientAccountIncorrect
        );
        let creator = self
            .get_account(tx.creator_id)
            .ok_or(MintNFTOpError::CreatorAccountNotFound)?;
        invariant!(
            creator.pub_key_hash != PubKeyHash::default(),
            MintNFTOpError::CreatorAccountIsLocked
        );

        if let Some((pub_key_hash, _)) = tx.verify_signature() {
            if pub_key_hash != creator.pub_key_hash {
                return Err(MintNFTOpError::InvalidSignature);
            }
        }

        let (recipient, _) = self
            .get_account_by_address(&tx.recipient)
            .ok_or(MintNFTOpError::RecipientAccountNotFound)?;

        let op = MintNFTOp {
            creator_account_id: tx.creator_id,
            recipient_account_id: recipient,
            tx,
        };

        Ok(op)
    }

    fn apply_tx(&mut self, tx: MintNFT) -> Result<OpSuccess, Self::OpError> {
        let op = self.create_op(tx)?;

        let (fee, updates) = <Self as TxHandler<MintNFT>>::apply_op(self, &op)?;
        let result = OpSuccess {
            fee,
            updates,
            executed_op: ZkSyncOp::MintNFTOp(Box::new(op)),
        };

        Ok(result)
    }

    fn apply_op(
        &mut self,
        op: &Self::Op,
    ) -> Result<(Option<CollectedFee>, AccountUpdates), Self::OpError> {
        let start = Instant::now();
        let mut updates = Vec::new();

        // The creator must pay fee for generating NFT.
        let mut creator_account = self
            .get_account(op.creator_account_id)
            .ok_or(MintNFTOpError::CreatorAccountNotFound)?;
        let old_balance = creator_account.get_balance(op.tx.fee_token);
        let nonce = creator_account.nonce;
        invariant!(nonce == op.tx.nonce, MintNFTOpError::NonceMismatch);

        invariant!(
            old_balance >= op.tx.fee,
            MintNFTOpError::InsufficientBalance
        );
        creator_account.sub_balance(op.tx.fee_token, &op.tx.fee);
        let new_balance = creator_account.get_balance(op.tx.fee_token);
        *creator_account.nonce += 1;
        updates.push((
            op.creator_account_id,
            AccountUpdate::UpdateBalance {
                balance_update: (op.tx.fee_token, old_balance, new_balance),
                old_nonce: nonce,
                new_nonce: creator_account.nonce,
            },
        ));
        self.insert_account(op.creator_account_id, creator_account.clone());

        // Serial ID is a counter in a special balance for NFT_TOKEN, which shows how many nft were generated by this creator
        let old_balance = creator_account.get_balance(NFT_TOKEN_ID);
        let old_nonce = creator_account.nonce;
        let serial_id = old_balance.to_u32().unwrap_or_default();
        creator_account.add_balance(NFT_TOKEN_ID, &BigUint::from(1u32));
        let new_balance = creator_account.get_balance(NFT_TOKEN_ID);
        updates.push((
            op.creator_account_id,
            AccountUpdate::UpdateBalance {
                balance_update: (NFT_TOKEN_ID, old_balance, new_balance),
                old_nonce,
                new_nonce: creator_account.nonce,
            },
        ));
        self.insert_account(op.creator_account_id, creator_account.clone());

        // The address for the nft token is generated based on `creator_account_id`,` serial_id` and `content_hash`
        // Generate token id. We have a special NFT account, which stores the next token id for nft in balance of NFT_TOKEN
        let (mut nft_account, account_updates) = self.get_or_create_nft_account_token_id();
        updates.extend(account_updates);

        let new_token_id = nft_account.get_balance(NFT_TOKEN_ID);
        nft_account.add_balance(NFT_TOKEN_ID, &BigUint::from(1u32));
        let next_token_id = nft_account.get_balance(NFT_TOKEN_ID);
        updates.push((
            NFT_STORAGE_ACCOUNT_ID,
            AccountUpdate::UpdateBalance {
                balance_update: (NFT_TOKEN_ID, new_token_id.clone(), next_token_id),
                old_nonce: Nonce(0),
                new_nonce: Nonce(0),
            },
        ));
        self.insert_account(NFT_STORAGE_ACCOUNT_ID, nft_account.clone());

        // Mint NFT with precalculated token_id, serial_id and address
        let token_id = TokenId(new_token_id.to_u32().expect("Should be correct u32"));
        let token_hash = calculate_token_hash(op.tx.creator_id, serial_id, op.tx.content_hash);
        let token_address = calculate_token_address(&token_hash);
        let token = NFT::new(
            token_id,
            serial_id,
            op.tx.creator_id,
            creator_account.address,
            token_address,
            None,
            op.tx.content_hash,
        );
        updates.push((
            op.creator_account_id,
            AccountUpdate::MintNFT {
                token: token.clone(),
                nonce,
            },
        ));
        self.nfts.insert(token_id, token);
        self.insert_account(op.creator_account_id, creator_account);

        // Token data is a special balance for NFT_STORAGE_ACCOUNT,
        // which represent last 16 bytes of hash of (account_id, serial_id, content_hash) for storing this data in circuit
        let token_data = calculate_token_data(&token_hash);
        let old_balance = nft_account.get_balance(token_id);
        assert_eq!(
            old_balance,
            BigUint::zero(),
            "The balance of nft token must be zero"
        );
        nft_account.add_balance(token_id, &token_data);
        updates.push((
            NFT_STORAGE_ACCOUNT_ID,
            AccountUpdate::UpdateBalance {
                balance_update: (token_id, BigUint::zero(), token_data),
                old_nonce: nft_account.nonce,
                new_nonce: nft_account.nonce,
            },
        ));
        self.insert_account(NFT_STORAGE_ACCOUNT_ID, nft_account);

        // Add this token to recipient account
        let mut recipient_account = self
            .get_account(op.recipient_account_id)
            .ok_or(MintNFTOpError::RecipientAccountNotFound)?;
        let old_amount = recipient_account.get_balance(token_id);
        invariant!(
            old_amount == BigUint::zero(),
            MintNFTOpError::TokenIsAlreadyInAccount
        );
        let old_nonce = recipient_account.nonce;
        recipient_account.add_balance(token_id, &BigUint::from(1u32));
        updates.push((
            op.recipient_account_id,
            AccountUpdate::UpdateBalance {
                balance_update: (token_id, BigUint::zero(), BigUint::from(1u32)),
                old_nonce,
                new_nonce: recipient_account.nonce,
            },
        ));
        self.insert_account(op.recipient_account_id, recipient_account);

        let fee = CollectedFee {
            token: op.tx.fee_token,
            amount: op.tx.fee.clone(),
        };

        metrics::histogram!("state.mint_nft", start.elapsed());
        Ok((Some(fee), updates))
    }
}
impl ZkSyncState {
    /// Get or create special account with special balance for enforcing uniqueness of token_id
    fn get_or_create_nft_account_token_id(&mut self) -> (Account, AccountUpdates) {
        let mut updates = vec![];
        let account = self.get_account(NFT_STORAGE_ACCOUNT_ID).unwrap_or_else(|| {
            vlog::error!("NFT Account is not defined in account tree, add it manually");
            let balance = BigUint::from(MIN_NFT_TOKEN_ID);
            let (mut account, upd) =
                Account::create_account(NFT_STORAGE_ACCOUNT_ID, *NFT_STORAGE_ACCOUNT_ADDRESS);
            updates.extend(upd.into_iter());
            account.add_balance(NFT_TOKEN_ID, &BigUint::from(MIN_NFT_TOKEN_ID));

            self.insert_account(NFT_STORAGE_ACCOUNT_ID, account.clone());

            updates.push((
                NFT_STORAGE_ACCOUNT_ID,
                AccountUpdate::UpdateBalance {
                    balance_update: (NFT_TOKEN_ID, BigUint::zero(), balance),
                    old_nonce: Nonce(0),
                    new_nonce: Nonce(0),
                },
            ));
            account
        });
        (account, updates)
    }
}

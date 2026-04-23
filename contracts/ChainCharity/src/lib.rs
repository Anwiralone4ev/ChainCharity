//! # Charity dApp Smart Contract — Multi-Sig 2/3 Edition
//! Stellar / Soroban · Rust
//!
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Quy trình phân bổ quỹ (2 bước · ngưỡng 2/3)               │
//! │                                                             │
//! │  Admin A  ──► create_proposal()  ──► Proposal { id, ... }  │
//! │  Admin B  ──► approve_proposal() ──► approvals = 1         │
//! │  Admin C  ──► approve_proposal() ──► approvals = 2  ──►    │
//! │                                       auto transfer() ✓    │
//! └─────────────────────────────────────────────────────────────┘
//!
//! Events phát ra:
//!   "donate"            (donor, amount, new_total)
//!   "proposal_created"  (proposal_id, proposer, recipient, amount)
//!   "proposal_approved" (proposal_id, approver, approval_count)
//!   "proposal_executed" (proposal_id, recipient, amount, remaining_total)
//!   "proposal_cancelled"(proposal_id, caller)

#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype,
    token::Client as TokenClient,
    vec, Address, Env, String, Symbol, Vec,
};

// ═══════════════════════════════════════════════════════════════
// Hằng số
// ═══════════════════════════════════════════════════════════════

/// Tổng số thành viên ban quản trị.
const ADMIN_COUNT: u32 = 3;

/// Số phiếu tối thiểu để thực thi proposal (2/3).
const APPROVAL_THRESHOLD: u32 = 2;

// ═══════════════════════════════════════════════════════════════
// StorageKey
// ═══════════════════════════════════════════════════════════════

/// Toàn bộ khoá lưu trữ của contract.
///
/// | Variant            | Storage     | Kiểu giá trị  |
/// |--------------------|-------------|---------------|
/// | Admins             | Instance    | Vec<Address>  |
/// | Token              | Instance    | Address       |
/// | TotalFunds         | Instance    | u128          |
/// | NextProposalId     | Instance    | u32           |
/// | Proposal(id)       | Persistent  | Proposal      |
/// | DonorBalance(addr) | Persistent  | u128          |
#[contracttype]
#[derive(Clone)]
pub enum StorageKey {
    /// Danh sách 3 địa chỉ ban quản trị.
    Admins,
    /// Địa chỉ token SEP-0041 được chấp nhận.
    Token,
    /// Tổng số token (u128) hiện có trong quỹ.
    TotalFunds,
    /// Bộ đếm ID — tự động tăng mỗi khi proposal mới được tạo.
    NextProposalId,
    /// Dữ liệu của một proposal theo ID.
    Proposal(u32),
    /// Lịch sử quyên góp của từng địa chỉ donor.
    DonorBalance(Address),
}

// ═══════════════════════════════════════════════════════════════
// ProposalStatus
// ═══════════════════════════════════════════════════════════════

/// Vòng đời của một đề xuất chi tiền.
#[contracttype]
#[derive(Clone, PartialEq)]
pub enum ProposalStatus {
    /// Đang chờ đủ phiếu phê duyệt.
    Pending,
    /// Đã đủ phiếu và đã được thực thi (tiền đã chuyển).
    Executed,
    /// Đã bị proposer huỷ trước khi đủ phiếu.
    Cancelled,
}

// ═══════════════════════════════════════════════════════════════
// Proposal
// ═══════════════════════════════════════════════════════════════

/// Đề xuất chi tiền — đơn vị trung tâm của cơ chế Multi-sig.
///
/// Được lưu trong Persistent storage theo khoá `Proposal(id)`.
#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    /// ID duy nhất, tự tăng từ 0.
    pub id: u32,
    /// Admin đã tạo đề xuất này (tự động có 1 phiếu đầu tiên).
    pub proposer: Address,
    /// Địa chỉ người thụ hưởng khi proposal được thực thi.
    pub recipient: Address,
    /// Số token cần chuyển.
    pub amount: u128,
    /// Mô tả mục đích (hash IPFS hoặc chuỗi UTF-8).
    pub description: String,
    /// Danh sách địa chỉ admin đã phê duyệt (không trùng lặp).
    pub approvals: Vec<Address>,
    /// Trạng thái hiện tại của proposal.
    pub status: ProposalStatus,
}

// ═══════════════════════════════════════════════════════════════
// Contract
// ═══════════════════════════════════════════════════════════════

#[contract]
pub struct CharityContract;

#[contractimpl]
impl CharityContract {
    // ───────────────────────────────────────────
    // 1. KHỞI TẠO
    // ───────────────────────────────────────────

    /// Khởi tạo contract với 3 admin và địa chỉ token.
    ///
    /// * `admin_a`, `admin_b`, `admin_c` — ba thành viên ban quản trị.
    /// * `token` — địa chỉ token SEP-0041.
    ///
    /// Chỉ được gọi **một lần**. Gọi lại panic với `already_initialized`.
    pub fn initialize(
        env: Env,
        admin_a: Address,
        admin_b: Address,
        admin_c: Address,
        token: Address,
    ) {
        if env.storage().instance().has(&StorageKey::Admins) {
            panic!("already_initialized");
        }

        // Ba admin phải là ba địa chỉ khác nhau
        if admin_a == admin_b || admin_b == admin_c || admin_a == admin_c {
            panic!("admins_must_be_distinct");
        }

        let admins: Vec<Address> = vec![&env, admin_a, admin_b, admin_c];

        env.storage().instance().set(&StorageKey::Admins, &admins);
        env.storage().instance().set(&StorageKey::Token, &token);
        env.storage().instance().set(&StorageKey::TotalFunds, &0_u128);
        env.storage().instance().set(&StorageKey::NextProposalId, &0_u32);
    }

    // ───────────────────────────────────────────
    // 2. QUYÊN GÓP
    // ───────────────────────────────────────────

    /// Quyên góp `amount` token vào quỹ từ thiện.
    ///
    /// Donor phải đã `approve` contract này trước khi gọi.
    /// Event: `"donate"` → `(amount, new_total)`.
    pub fn donate(env: Env, donor: Address, amount: u128) {
        donor.require_auth();

        if amount == 0 {
            panic!("amount_must_be_positive");
        }

        TokenClient::new(&env, &Self::get_token(&env)).transfer(
            &donor,
            &env.current_contract_address(),
            &(amount as i128),
        );

        let new_total = Self::add_to_total(&env, amount);

        let key = StorageKey::DonorBalance(donor.clone());
        let prev: u128 = env.storage().persistent().get(&key).unwrap_or(0_u128);
        env.storage().persistent().set(&key, &(prev + amount));

        env.events().publish(
            (Symbol::new(&env, "donate"), donor),
            (amount, new_total),
        );
    }

    // ───────────────────────────────────────────
    // 3. TẠO ĐỀ XUẤT  [Bước 1 / 2]
    // ───────────────────────────────────────────

    /// Admin tạo đề xuất chi tiền — Bước 1 của quy trình multi-sig.
    ///
    /// Người tạo tự động được tính là phiếu đầu tiên.
    /// Nếu `APPROVAL_THRESHOLD == 1`, proposal thực thi ngay.
    ///
    /// Trả về `proposal_id` để các admin khác dùng ở Bước 2.
    ///
    /// Event: `"proposal_created"` → `(proposal_id, recipient, amount, description)`.
    pub fn create_proposal(
        env: Env,
        proposer: Address,
        recipient: Address,
        amount: u128,
        description: String,
    ) -> u32 {
        proposer.require_auth();
        Self::assert_is_admin(&env, &proposer);

        if amount == 0 {
            panic!("amount_must_be_positive");
        }

        // Fail-fast: kiểm tra số dư tại thời điểm tạo
        if amount > Self::read_total_funds(&env) {
            panic!("insufficient_funds");
        }

        let proposal_id = Self::next_proposal_id(&env);

        // Proposer tự động có 1 phiếu
        let initial_approvals: Vec<Address> = vec![&env, proposer.clone()];

        let proposal = Proposal {
            id: proposal_id,
            proposer: proposer.clone(),
            recipient: recipient.clone(),
            amount,
            description: description.clone(),
            approvals: initial_approvals,
            status: ProposalStatus::Pending,
        };

        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "proposal_created"), proposer.clone()),
            (proposal_id, recipient, amount, description),
        );

        // Edge case: ngưỡng = 1 thì thực thi luôn
        if APPROVAL_THRESHOLD <= 1 {
            Self::execute_proposal(&env, proposal_id);
        }

        proposal_id
    }

    // ───────────────────────────────────────────
    // 4. PHÊ DUYỆT ĐỀ XUẤT  [Bước 2 / 2]
    // ───────────────────────────────────────────

    /// Admin phê duyệt một proposal đang `Pending` — Bước 2 của quy trình.
    ///
    /// Kiểm tra bảo mật (theo thứ tự):
    /// 1. `approver` phải là admin hợp lệ.
    /// 2. Proposal phải đang `Pending`.
    /// 3. Mỗi admin chỉ được phê duyệt **một lần** (chống double-vote).
    ///
    /// Khi tổng phiếu ≥ `APPROVAL_THRESHOLD`:
    /// → Trạng thái chuyển `Executed` → token được transfer tự động.
    ///
    /// Event: `"proposal_approved"` → `(proposal_id, approval_count)`.
    /// Event (nếu đủ ngưỡng): `"proposal_executed"` → `(proposal_id, amount, remaining)`.
    pub fn approve_proposal(env: Env, approver: Address, proposal_id: u32) {
        approver.require_auth();
        Self::assert_is_admin(&env, &approver);

        let mut proposal = Self::load_proposal(&env, proposal_id);

        if proposal.status != ProposalStatus::Pending {
            panic!("proposal_not_pending");
        }

        // Chống double-vote: kiểm tra approver đã có trong danh sách chưa
        for existing in proposal.approvals.iter() {
            if existing == approver {
                panic!("already_approved");
            }
        }

        proposal.approvals.push_back(approver.clone());
        let approval_count = proposal.approvals.len();

        // Ghi trạng thái trung gian
        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "proposal_approved"), approver.clone()),
            (proposal_id, approval_count),
        );

        // Kích hoạt thực thi nếu đủ ngưỡng
        if approval_count >= APPROVAL_THRESHOLD {
            Self::execute_proposal(&env, proposal_id);
        }
    }

    // ───────────────────────────────────────────
    // 5. HUỶ ĐỀ XUẤT
    // ───────────────────────────────────────────

    /// Proposer huỷ đề xuất của chính mình khi chưa đủ phiếu.
    ///
    /// Chỉ proposer gốc mới được phép huỷ.
    /// Proposal phải đang ở trạng thái `Pending`.
    ///
    /// Event: `"proposal_cancelled"` → `(proposal_id,)`.
    pub fn cancel_proposal(env: Env, caller: Address, proposal_id: u32) {
        caller.require_auth();
        Self::assert_is_admin(&env, &caller);

        let mut proposal = Self::load_proposal(&env, proposal_id);

        if proposal.status != ProposalStatus::Pending {
            panic!("proposal_not_pending");
        }

        if caller != proposal.proposer {
            panic!("only_proposer_can_cancel");
        }

        proposal.status = ProposalStatus::Cancelled;

        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "proposal_cancelled"), caller),
            (proposal_id,),
        );
    }

    // ───────────────────────────────────────────
    // 6. VIEW FUNCTIONS
    // ───────────────────────────────────────────

    /// Trả về tổng quỹ hiện tại.
    pub fn get_total_funds(env: Env) -> u128 {
        Self::read_total_funds(&env)
    }

    /// Trả về thông tin đầy đủ của một proposal theo ID.
    pub fn get_proposal(env: Env, proposal_id: u32) -> Proposal {
        Self::load_proposal(&env, proposal_id)
    }

    /// Trả về ID của proposal sẽ được tạo tiếp theo.
    pub fn get_next_proposal_id(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&StorageKey::NextProposalId)
            .unwrap_or(0_u32)
    }

    /// Trả về danh sách 3 admin.
    pub fn get_admins(env: Env) -> Vec<Address> {
        Self::read_admins(&env)
    }

    /// Trả về tổng số tiền đã quyên góp của một donor.
    pub fn get_donor_balance(env: Env, donor: Address) -> u128 {
        env.storage()
            .persistent()
            .get(&StorageKey::DonorBalance(donor))
            .unwrap_or(0_u128)
    }

    // ───────────────────────────────────────────
    // 7. INTERNAL HELPERS
    // ───────────────────────────────────────────

    /// Thực thi transfer khi proposal đủ ngưỡng phê duyệt.
    ///
    /// Thứ tự chống re-entrancy:
    ///   1. Đánh dấu `Executed` → ghi storage
    ///   2. Trừ TotalFunds → ghi storage
    ///   3. Gọi TokenClient::transfer()
    ///   4. Phát event
    fn execute_proposal(env: &Env, proposal_id: u32) {
        let mut proposal = Self::load_proposal(env, proposal_id);

        // Kiểm tra lại số dư tại thời điểm thực thi
        let total = Self::read_total_funds(env);
        if proposal.amount > total {
            panic!("insufficient_funds_at_execution");
        }

        // ① Đánh dấu Executed TRƯỚC — chống re-entrancy
        proposal.status = ProposalStatus::Executed;
        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(proposal_id), &proposal);

        // ② Trừ quỹ
        let remaining = total - proposal.amount;
        env.storage()
            .instance()
            .set(&StorageKey::TotalFunds, &remaining);

        // ③ Transfer token
        TokenClient::new(env, &Self::get_token(env)).transfer(
            &env.current_contract_address(),
            &proposal.recipient,
            &(proposal.amount as i128),
        );

        // ④ Event
        env.events().publish(
            (
                Symbol::new(env, "proposal_executed"),
                proposal.recipient.clone(),
            ),
            (proposal_id, proposal.amount, remaining),
        );
    }

    /// Kiểm tra `addr` có trong danh sách admins; panic `not_admin` nếu không.
    fn assert_is_admin(env: &Env, addr: &Address) {
        for admin in Self::read_admins(env).iter() {
            if &admin == addr {
                return;
            }
        }
        panic!("not_admin");
    }

    fn read_admins(env: &Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&StorageKey::Admins)
            .expect("not_initialized")
    }

    fn get_token(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&StorageKey::Token)
            .expect("not_initialized")
    }

    fn read_total_funds(env: &Env) -> u128 {
        env.storage()
            .instance()
            .get(&StorageKey::TotalFunds)
            .unwrap_or(0_u128)
    }

    fn load_proposal(env: &Env, proposal_id: u32) -> Proposal {
        env.storage()
            .persistent()
            .get(&StorageKey::Proposal(proposal_id))
            .expect("proposal_not_found")
    }

    /// Lấy ID tiếp theo và tăng bộ đếm lên 1.
    fn next_proposal_id(env: &Env) -> u32 {
        let id: u32 = env
            .storage()
            .instance()
            .get(&StorageKey::NextProposalId)
            .unwrap_or(0_u32);
        env.storage()
            .instance()
            .set(&StorageKey::NextProposalId, &(id + 1));
        id
    }

    fn add_to_total(env: &Env, amount: u128) -> u128 {
        let current = Self::read_total_funds(env);
        let new_total = current.checked_add(amount).expect("total_funds_overflow");
        env.storage()
            .instance()
            .set(&StorageKey::TotalFunds, &new_total);
        new_total
    }
}

// ═══════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _,
        token::{Client as TokenClient, StellarAssetClient},
        Address, Env, String,
    };

    // ── Fixture ──────────────────────────────────────────────

    /// Trả về (env, contract_id, token, admin_a, admin_b, admin_c, donor).
    fn setup() -> (Env, Address, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let token_admin = Address::generate(&env);
        let token_sac = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token = token_sac.address();

        let admin_a = Address::generate(&env);
        let admin_b = Address::generate(&env);
        let admin_c = Address::generate(&env);

        let contract_id = env.register(CharityContract, ());
        let client = CharityContractClient::new(&env, &contract_id);
        client.initialize(&admin_a, &admin_b, &admin_c, &token);

        let donor = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&donor, &2_000_000_i128);

        (env, contract_id, token, admin_a, admin_b, admin_c, donor)
    }

    // ── Donate ───────────────────────────────────────────────

    #[test]
    fn test_donate_records_balance() {
        let (env, cid, _t, _a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);

        client.donate(&donor, &1_000_000_u128);

        assert_eq!(client.get_total_funds(), 1_000_000_u128);
        assert_eq!(client.get_donor_balance(&donor), 1_000_000_u128);
    }

    #[test]
    #[should_panic(expected = "amount_must_be_positive")]
    fn test_donate_zero_rejected() {
        let (env, cid, _t, _a, _b, _c, donor) = setup();
        CharityContractClient::new(&env, &cid).donate(&donor, &0_u128);
    }

    // ── Multi-sig happy path ──────────────────────────────────

    #[test]
    fn test_multisig_2_of_3_executes() {
        let (env, cid, _t, admin_a, admin_b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &1_000_000_u128);

        // Bước 1: Admin A tạo → 1 phiếu tự động
        let pid = client.create_proposal(
            &admin_a,
            &recipient,
            &400_000_u128,
            &String::from_str(&env, "QmIPFS_truong_vung_cao"),
        );
        assert_eq!(pid, 0_u32);

        let p = client.get_proposal(&pid);
        assert_eq!(p.status, ProposalStatus::Pending);
        assert_eq!(p.approvals.len(), 1);

        // Bước 2: Admin B phê duyệt → đủ ngưỡng → auto-execute
        client.approve_proposal(&admin_b, &pid);

        let p_after = client.get_proposal(&pid);
        assert_eq!(p_after.status, ProposalStatus::Executed);
        assert_eq!(client.get_total_funds(), 600_000_u128);
    }

    #[test]
    fn test_one_approval_stays_pending() {
        let (env, cid, _t, admin_a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &500_000_u128);

        let pid = client.create_proposal(
            &admin_a,
            &recipient,
            &200_000_u128,
            &String::from_str(&env, "phase_1"),
        );

        // Chỉ 1 phiếu → vẫn Pending, quỹ chưa bị trừ
        assert_eq!(client.get_proposal(&pid).status, ProposalStatus::Pending);
        assert_eq!(client.get_total_funds(), 500_000_u128);
    }

    // ── Bảo mật: double-vote ──────────────────────────────────

    #[test]
    #[should_panic(expected = "already_approved")]
    fn test_double_vote_rejected() {
        let (env, cid, _t, admin_a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &500_000_u128);
        let pid = client.create_proposal(
            &admin_a, &recipient, &100_000_u128,
            &String::from_str(&env, "test"),
        );
        // Admin A vote lần 2 → panic
        client.approve_proposal(&admin_a, &pid);
    }

    // ── Bảo mật: không phải admin ────────────────────────────

    #[test]
    #[should_panic(expected = "not_admin")]
    fn test_non_admin_cannot_create() {
        let (env, cid, _t, _a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &500_000_u128);
        client.create_proposal(
            &donor, &recipient, &100_000_u128,
            &String::from_str(&env, "hack"),
        );
    }

    #[test]
    #[should_panic(expected = "not_admin")]
    fn test_non_admin_cannot_approve() {
        let (env, cid, _t, admin_a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &500_000_u128);
        let pid = client.create_proposal(
            &admin_a, &recipient, &100_000_u128,
            &String::from_str(&env, "test"),
        );
        client.approve_proposal(&donor, &pid);
    }

    // ── Bảo mật: approve sau Executed ────────────────────────

    #[test]
    #[should_panic(expected = "proposal_not_pending")]
    fn test_approve_executed_rejected() {
        let (env, cid, _t, admin_a, admin_b, admin_c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &1_000_000_u128);
        let pid = client.create_proposal(
            &admin_a, &recipient, &500_000_u128,
            &String::from_str(&env, "test"),
        );
        client.approve_proposal(&admin_b, &pid); // → Executed

        // Admin C cố approve sau khi đã Executed
        client.approve_proposal(&admin_c, &pid);
    }

    // ── Bảo mật: vượt số dư ──────────────────────────────────

    #[test]
    #[should_panic(expected = "insufficient_funds")]
    fn test_proposal_exceeds_balance() {
        let (env, cid, _t, admin_a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &100_u128);
        client.create_proposal(
            &admin_a, &recipient, &999_u128,
            &String::from_str(&env, "over_budget"),
        );
    }

    // ── Huỷ proposal ─────────────────────────────────────────

    #[test]
    fn test_proposer_can_cancel() {
        let (env, cid, _t, admin_a, _b, _c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let recipient = Address::generate(&env);

        client.donate(&donor, &500_000_u128);
        let pid = client.create_proposal(
            &admin_a, &recipient, &200_000_u128,
            &String::from_str(&env, "cancel_me"),
        );
        client.cancel_proposal(&admin_a, &pid);

        assert_eq!(client.get_proposal(&pid).status, ProposalStatus::Cancelled);
        // Quỹ không bị trừ
        assert_eq!(client.get_total_funds(), 500_000_u128);
    }

    // ── Nhiều proposal song song ──────────────────────────────

    #[test]
    fn test_multiple_proposals_independent() {
        let (env, cid, _t, admin_a, admin_b, admin_c, donor) = setup();
        let client = CharityContractClient::new(&env, &cid);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);

        client.donate(&donor, &2_000_000_u128);

        let pid0 = client.create_proposal(
            &admin_a, &r1, &300_000_u128,
            &String::from_str(&env, "project_A"),
        );
        let pid1 = client.create_proposal(
            &admin_b, &r2, &500_000_u128,
            &String::from_str(&env, "project_B"),
        );

        client.approve_proposal(&admin_b, &pid0); // pid0 executed
        client.approve_proposal(&admin_a, &pid1); // pid1 executed

        assert_eq!(client.get_total_funds(), 1_200_000_u128);
        assert_eq!(client.get_proposal(&pid0).status, ProposalStatus::Executed);
        assert_eq!(client.get_proposal(&pid1).status, ProposalStatus::Executed);
    }

    // ── get_admins ────────────────────────────────────────────

    #[test]
    fn test_get_admins_returns_three() {
        let (env, cid, _t, _a, _b, _c, _d) = setup();
        let client = CharityContractClient::new(&env, &cid);
        assert_eq!(client.get_admins().len(), 3);
    }
}
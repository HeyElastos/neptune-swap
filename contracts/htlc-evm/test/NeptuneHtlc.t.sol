// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {NeptuneHtlc} from "../src/NeptuneHtlc.sol";

/// Foundry's cheatcodes, the few these tests use (no forge-std dependency).
interface Vm {
    function warp(uint256) external;
    function deal(address, uint256) external;
    function prank(address) external;
    function expectRevert(bytes4) external;
    function expectEmit(bool, bool, bool, bool) external;
}

contract Token {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external virtual returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external virtual returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/// Keeps 1 % of every transferFrom.
contract FeeToken is Token {
    function transferFrom(address from, address to, uint256 amount) external override returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount - amount / 100;
        return true;
    }
}

/// A receiver that tries to claim again while being paid.
contract Reenterer {
    NeptuneHtlc htlc;
    bytes32 id;
    bytes32 preimage;
    bool public reentryFailed;

    constructor(NeptuneHtlc h) {
        htlc = h;
    }

    function arm(bytes32 i, bytes32 p) external {
        (id, preimage) = (i, p);
    }

    receive() external payable {
        try htlc.claim(id, preimage) {} catch {
            reentryFailed = true;
        }
    }
}

contract NeptuneHtlcTest {
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    NeptuneHtlc htlc;
    address constant ALICE = address(0xA11CE); // locks (the giver on this chain)
    address constant BOB = address(0xB0B); // claims
    bytes32 constant SECRET = bytes32(uint256(0x5ec2e7));
    bytes32 hashLock;
    uint64 T;

    function setUp() public {
        htlc = new NeptuneHtlc();
        hashLock = sha256(abi.encodePacked(SECRET));
        vm.warp(1_800_000_000);
        T = uint64(block.timestamp + 3600);
        vm.deal(ALICE, 10 ether);
    }

    function eq(uint256 a, uint256 b, string memory what) internal pure {
        require(a == b, what);
    }

    function lockEth(uint256 amount) internal returns (bytes32 id) {
        vm.prank(ALICE);
        id = htlc.lock{value: amount}(BOB, hashLock, T);
    }

    function test_hash_lock_is_sha256_of_the_32_byte_secret() public pure {
        // The same vector the Neptune lock script is held to: SHA-256 of 32 raw bytes.
        require(
            sha256(abi.encodePacked(bytes32(0))) == 0x66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925,
            "sha256 vector"
        );
    }

    function test_claim_before_T_pays_the_receiver_and_publishes_the_secret() public {
        bytes32 id = lockEth(1 ether);
        eq(address(htlc).balance, 1 ether, "locked");
        vm.expectEmit(true, false, false, true);
        emit NeptuneHtlc.Claimed(id, SECRET);
        htlc.claim(id, SECRET); // anyone may submit it
        eq(BOB.balance, 1 ether, "receiver paid");
        eq(address(htlc).balance, 0, "nothing left");
        vm.expectRevert(NeptuneHtlc.AlreadySpent.selector);
        htlc.claim(id, SECRET);
        vm.warp(T);
        vm.expectRevert(NeptuneHtlc.AlreadySpent.selector);
        htlc.refund(id);
    }

    function test_a_wrong_secret_is_refused() public {
        bytes32 id = lockEth(1 ether);
        vm.expectRevert(NeptuneHtlc.WrongPreimage.selector);
        htlc.claim(id, bytes32(uint256(1)));
        vm.expectRevert(NeptuneHtlc.WrongPreimage.selector);
        htlc.claim(id, hashLock); // the hash itself is not the secret
    }

    function test_the_claim_closes_at_T_and_the_refund_opens_at_T() public {
        bytes32 id = lockEth(1 ether);
        vm.warp(T - 1);
        vm.expectRevert(NeptuneHtlc.RefundNotOpen.selector);
        htlc.refund(id);
        vm.warp(T);
        vm.expectRevert(NeptuneHtlc.ClaimWindowClosed.selector);
        htlc.claim(id, SECRET);
        htlc.refund(id); // anyone may submit it; the sender is paid
        eq(ALICE.balance, 10 ether, "sender refunded");
        vm.expectRevert(NeptuneHtlc.AlreadySpent.selector);
        htlc.refund(id);
    }

    function test_bad_locks_are_refused() public {
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.InvalidLock.selector);
        htlc.lock{value: 1 ether}(address(0), hashLock, T);
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.InvalidLock.selector);
        htlc.lock{value: 0}(BOB, hashLock, T);
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.InvalidLock.selector);
        htlc.lock{value: 1 ether}(BOB, bytes32(0), T);
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.InvalidLock.selector);
        htlc.lock{value: 1 ether}(BOB, hashLock, uint64(block.timestamp));
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.InvalidLock.selector);
        htlc.lockToken(address(0), 1, BOB, hashLock, T);
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.InvalidLock.selector);
        htlc.lockToken(BOB, 1, BOB, hashLock, T); // not a contract
    }

    function test_unknown_and_duplicate_locks_are_refused() public {
        vm.expectRevert(NeptuneHtlc.NoSuchLock.selector);
        htlc.claim(bytes32(uint256(7)), SECRET);
        vm.expectRevert(NeptuneHtlc.NoSuchLock.selector);
        htlc.refund(bytes32(uint256(7)));
        lockEth(1 ether);
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.LockExists.selector);
        htlc.lock{value: 1 ether}(BOB, hashLock, T);
    }

    function test_a_reentering_receiver_is_paid_once() public {
        Reenterer r = new Reenterer(htlc);
        vm.prank(ALICE);
        bytes32 id = htlc.lock{value: 1 ether}(address(r), hashLock, T);
        r.arm(id, SECRET);
        htlc.claim(id, SECRET);
        eq(address(r).balance, 1 ether, "paid once");
        require(r.reentryFailed(), "the second claim failed inside");
    }

    function test_tokens_lock_claim_and_refund() public {
        Token usdc = new Token();
        usdc.mint(ALICE, 1000e6);
        vm.prank(ALICE);
        usdc.approve(address(htlc), 300e6);
        vm.prank(ALICE);
        bytes32 id = htlc.lockToken(address(usdc), 250e6, BOB, hashLock, T);
        eq(usdc.balanceOf(address(htlc)), 250e6, "token locked");
        htlc.claim(id, SECRET);
        eq(usdc.balanceOf(BOB), 250e6, "token claimed");

        bytes32 hash2 = sha256(abi.encodePacked(bytes32(uint256(2))));
        vm.prank(ALICE);
        bytes32 id2 = htlc.lockToken(address(usdc), 50e6, BOB, hash2, T);
        vm.warp(T);
        htlc.refund(id2);
        eq(usdc.balanceOf(ALICE), 750e6, "token refunded");
    }

    function test_a_token_that_delivers_less_is_refused() public {
        FeeToken fee = new FeeToken();
        fee.mint(ALICE, 1000);
        vm.prank(ALICE);
        fee.approve(address(htlc), 1000);
        vm.prank(ALICE);
        vm.expectRevert(NeptuneHtlc.TransferFailed.selector);
        htlc.lockToken(address(fee), 1000, BOB, hashLock, T);
    }
}

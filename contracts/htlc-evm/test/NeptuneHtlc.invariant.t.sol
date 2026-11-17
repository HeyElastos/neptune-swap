// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {NeptuneHtlc} from "../src/NeptuneHtlc.sol";

/// The cheatcodes these tests use (no forge-std dependency).
interface Hevm {
    function warp(uint256) external;
    function deal(address, uint256) external;
    function prank(address) external;
}

Hevm constant HEVM = Hevm(address(uint160(uint256(keccak256("hevm cheat code")))));

/// Drives the contract with random locks, claims (right and wrong secrets) and refunds (early
/// and late) from several parties, and keeps its own books of what each party must hold.
/// Every call that the rules forbid must revert; every call they allow must pay exactly the
/// right party. The invariants below then hold the contract to those books.
contract Handler {
    NeptuneHtlc public immutable htlc;
    address[3] public parties = [address(0xA11CE), address(0xB0B), address(0xCA401)];
    uint256 public constant START = 1_000 ether;

    struct Open {
        bytes32 id;
        address sender;
        address receiver;
        uint256 amount;
        bytes32 secret;
        uint64 refundAfter;
    }

    Open[] public open;
    /// Locks already claimed or refunded, with their secrets: spending one again must fail.
    Open[] public closed;
    /// What each party should hold, by these books.
    mapping(address => uint256) public expected;
    uint256 public lockedTotal;
    uint256 public paidTwice;
    uint256 public wrongPayouts;

    constructor(NeptuneHtlc h) {
        htlc = h;
        HEVM.warp(1_800_000_000);
        for (uint256 i; i < 3; i++) {
            HEVM.deal(parties[i], START);
            expected[parties[i]] = START;
        }
    }

    function openCount() external view returns (uint256) {
        return open.length;
    }

    function lock(uint256 from, uint256 to, uint256 amount, bytes32 secret, uint32 duration) external {
        address sender = parties[from % 3];
        address receiver = parties[to % 3];
        amount = 1 + amount % (expected[sender] / 4 + 1);
        if (amount > expected[sender]) return;
        uint64 refundAfter = uint64(block.timestamp) + 1 + uint64(duration % 7 days);
        bytes32 hashLock = sha256(abi.encodePacked(secret));
        HEVM.prank(sender);
        try htlc.lock{value: amount}(receiver, hashLock, refundAfter) returns (bytes32 id) {
            open.push(Open(id, sender, receiver, amount, secret, refundAfter));
            expected[sender] -= amount;
            lockedTotal += amount;
        } catch {}
    }

    function claim(uint256 which, uint256 caller, bool rightSecret, bytes32 wrong) external {
        if (open.length == 0) return;
        uint256 i = which % open.length;
        Open memory o = open[i];
        bytes32 preimage = rightSecret ? o.secret : wrong;
        bool allowed = (preimage == o.secret || sha256(abi.encodePacked(preimage)) == sha256(abi.encodePacked(o.secret)))
            && block.timestamp < o.refundAfter;
        uint256 before = o.receiver.balance;
        HEVM.prank(parties[caller % 3]);
        try htlc.claim(o.id, preimage) {
            if (!allowed) wrongPayouts++;
            if (o.receiver.balance != before + o.amount && o.receiver != parties[caller % 3]) wrongPayouts++;
            expected[o.receiver] += o.amount;
            lockedTotal -= o.amount;
            _close(i);
        } catch {
            if (allowed) wrongPayouts++; // an allowed claim must go through
        }
    }

    function refund(uint256 which, uint256 caller) external {
        if (open.length == 0) return;
        uint256 i = which % open.length;
        Open memory o = open[i];
        bool allowed = block.timestamp >= o.refundAfter;
        HEVM.prank(parties[caller % 3]);
        try htlc.refund(o.id) {
            if (!allowed) wrongPayouts++;
            expected[o.sender] += o.amount;
            lockedTotal -= o.amount;
            _close(i);
        } catch {
            if (allowed) wrongPayouts++;
        }
    }

    /// Spending a closed lock again (claim with its real secret, or refund) must always fail.
    function spendAgain(uint256 which, uint256 caller) external {
        if (closed.length == 0) return;
        Open memory o = closed[which % closed.length];
        HEVM.prank(parties[caller % 3]);
        try htlc.claim(o.id, o.secret) {
            paidTwice++;
        } catch {}
        HEVM.prank(parties[caller % 3]);
        try htlc.refund(o.id) {
            paidTwice++;
        } catch {}
    }

    /// A claim or refund of a lock that was never made must fail.
    function spendUnknown(bytes32 id, bytes32 preimage) external {
        for (uint256 i; i < open.length; i++) if (open[i].id == id) return;
        for (uint256 i; i < closed.length; i++) if (closed[i].id == id) return;
        try htlc.claim(id, preimage) {
            wrongPayouts++;
        } catch {}
        try htlc.refund(id) {
            wrongPayouts++;
        } catch {}
    }

    function wait(uint32 secs) external {
        HEVM.warp(block.timestamp + secs % 2 days);
    }

    function _close(uint256 i) internal {
        closed.push(open[i]);
        open[i] = open[open.length - 1];
        open.pop();
    }
}

contract NeptuneHtlcInvariantTest {
    NeptuneHtlc htlc;
    Handler handler;

    function setUp() public {
        htlc = new NeptuneHtlc();
        handler = new Handler(htlc);
    }

    /// Foundry targets the contracts made in setUp; only the handler should be called.
    function targetContracts() public view returns (address[] memory targets) {
        targets = new address[](1);
        targets[0] = address(handler);
    }

    /// The contract holds exactly what is locked and unspent: nothing more, nothing less.
    function invariant_holds_exactly_the_open_locks() public view {
        require(address(htlc).balance == handler.lockedTotal(), "balance != open locks");
    }

    /// Every party holds exactly what the rules say: no one ever gained or lost otherwise.
    function invariant_every_party_holds_what_the_rules_give_it() public view {
        for (uint256 i; i < 3; i++) {
            address p = handler.parties(i);
            require(p.balance == handler.expected(p), "a party's balance broke the rules");
        }
    }

    /// No forbidden claim or refund went through, and no allowed one failed.
    function invariant_only_the_rules_move_money() public view {
        require(handler.wrongPayouts() == 0, "money moved against the rules");
        require(handler.paidTwice() == 0, "a lock paid twice");
    }

    /// Stateless fuzz: only the exact secret opens a lock, and only before its time.
    function testFuzz_only_the_secret_opens_a_lock(bytes32 secret, bytes32 other, uint32 duration, uint32 later) public {
        HEVM.warp(1_800_000_000);
        // Fresh addresses: the handler's parties hold play money already.
        address alice = address(0xF00A);
        address bob = address(0xF00B);
        HEVM.deal(alice, 1 ether);
        uint64 refundAfter = uint64(block.timestamp) + 1 + uint64(duration % 7 days);
        bytes32 hashLock = sha256(abi.encodePacked(secret)); // before prank: a precompile call would use it up
        HEVM.prank(alice);
        bytes32 id = htlc.lock{value: 1 ether}(bob, hashLock, refundAfter);
        if (other != secret) {
            try htlc.claim(id, other) {
                revert("a wrong secret opened the lock");
            } catch {}
        }
        HEVM.warp(block.timestamp + later % 14 days);
        if (block.timestamp < refundAfter) {
            try htlc.refund(id) {
                revert("refunded early");
            } catch {}
            htlc.claim(id, secret);
            require(bob.balance == 1 ether, "the receiver was not paid");
        } else {
            try htlc.claim(id, secret) {
                revert("claimed after the refund time");
            } catch {}
            htlc.refund(id);
            require(alice.balance == 1 ether, "the sender was not refunded");
        }
        require(address(htlc).balance == 0, "money left behind");
    }
}

// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @title The EVM half of a Neptune atomic swap: a hashed timelock for ETH or an ERC-20 token.
/// @notice The hash lock is SHA-256 of a 32-byte secret, the same lock the Neptune HTLC lock
/// script checks, so one secret opens both legs. A lock is claimable by its receiver with the
/// secret strictly before `refundAfter`, and refundable to its sender from `refundAfter` on.
/// Claiming publishes the secret in the `Claimed` event; that is how the other side learns it.
/// There is no owner, no fee and no upgrade path.
contract NeptuneHtlc {
    struct Lock {
        address sender;
        address receiver;
        /// address(0) for ETH.
        address token;
        uint256 amount;
        bytes32 hashLock;
        /// Unix seconds (block.timestamp) from which the refund opens and the claim closes.
        uint64 refundAfter;
        bool spent;
    }

    mapping(bytes32 => Lock) public locks;

    event Locked(
        bytes32 indexed id,
        address indexed sender,
        address indexed receiver,
        address token,
        uint256 amount,
        bytes32 hashLock,
        uint64 refundAfter
    );
    event Claimed(bytes32 indexed id, bytes32 preimage);
    event Refunded(bytes32 indexed id);

    error InvalidLock();
    error LockExists();
    error NoSuchLock();
    error AlreadySpent();
    error WrongPreimage();
    error ClaimWindowClosed();
    error RefundNotOpen();
    error TransferFailed();

    /// @notice The id a lock with these terms gets. The hash lock is unique per swap.
    function lockId(
        address sender,
        address receiver,
        address token,
        uint256 amount,
        bytes32 hashLock,
        uint64 refundAfter
    ) public pure returns (bytes32) {
        return keccak256(abi.encode(sender, receiver, token, amount, hashLock, refundAfter));
    }

    /// @notice Locks the ETH sent with the call.
    function lock(address receiver, bytes32 hashLock, uint64 refundAfter) external payable returns (bytes32 id) {
        id = _open(receiver, address(0), msg.value, hashLock, refundAfter);
    }

    /// @notice Locks `amount` of `token`, which the sender has approved this contract to move.
    /// Tokens that deliver less than `amount` (fee-on-transfer) are refused.
    function lockToken(address token, uint256 amount, address receiver, bytes32 hashLock, uint64 refundAfter)
        external
        returns (bytes32 id)
    {
        if (token == address(0) || token.code.length == 0) revert InvalidLock();
        id = _open(receiver, token, amount, hashLock, refundAfter);
        uint256 before = _balanceOf(token, address(this));
        _call(token, abi.encodeWithSelector(0x23b872dd, msg.sender, address(this), amount)); // transferFrom
        if (_balanceOf(token, address(this)) - before != amount) revert TransferFailed();
    }

    /// @notice Pays the lock to its receiver. Anyone may submit the secret; the funds go only to
    /// the receiver.
    function claim(bytes32 id, bytes32 preimage) external {
        Lock storage l = locks[id];
        if (l.sender == address(0)) revert NoSuchLock();
        if (l.spent) revert AlreadySpent();
        if (block.timestamp >= l.refundAfter) revert ClaimWindowClosed();
        if (sha256(abi.encodePacked(preimage)) != l.hashLock) revert WrongPreimage();
        l.spent = true;
        emit Claimed(id, preimage);
        _pay(l.token, l.receiver, l.amount);
    }

    /// @notice Returns the lock to its sender from `refundAfter` on. Anyone may submit it.
    function refund(bytes32 id) external {
        Lock storage l = locks[id];
        if (l.sender == address(0)) revert NoSuchLock();
        if (l.spent) revert AlreadySpent();
        if (block.timestamp < l.refundAfter) revert RefundNotOpen();
        l.spent = true;
        emit Refunded(id);
        _pay(l.token, l.sender, l.amount);
    }

    function _open(address receiver, address token, uint256 amount, bytes32 hashLock, uint64 refundAfter)
        private
        returns (bytes32 id)
    {
        if (receiver == address(0) || amount == 0 || hashLock == bytes32(0) || refundAfter <= block.timestamp) {
            revert InvalidLock();
        }
        id = lockId(msg.sender, receiver, token, amount, hashLock, refundAfter);
        if (locks[id].sender != address(0)) revert LockExists();
        locks[id] = Lock(msg.sender, receiver, token, amount, hashLock, refundAfter, false);
        emit Locked(id, msg.sender, receiver, token, amount, hashLock, refundAfter);
    }

    function _pay(address token, address to, uint256 amount) private {
        if (token == address(0)) {
            (bool ok,) = to.call{value: amount}("");
            if (!ok) revert TransferFailed();
        } else {
            _call(token, abi.encodeWithSelector(0xa9059cbb, to, amount)); // transfer
        }
    }

    /// A token call that must not revert and, if it returns data, must return true (USDT-style
    /// tokens return nothing).
    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory ret) = token.call(data);
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TransferFailed();
    }

    function _balanceOf(address token, address who) private view returns (uint256) {
        (bool ok, bytes memory ret) = token.staticcall(abi.encodeWithSelector(0x70a08231, who)); // balanceOf
        if (!ok || ret.length < 32) revert TransferFailed();
        return abi.decode(ret, (uint256));
    }
}

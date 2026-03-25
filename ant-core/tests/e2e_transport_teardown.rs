// Removed — the regression test proved that incomplete P2PNode teardown
// caused QUIC transport failures. The fix (calling p2p_node.shutdown()
// in MiniTestnet::teardown) is now in place.

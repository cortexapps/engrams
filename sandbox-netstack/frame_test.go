package main

import (
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"net/netip"
	"syscall"
	"testing"
)

func testNetwork() network {
	return network{netip.MustParseAddr("192.0.2.2"), netip.MustParseAddr("192.0.2.1"), [6]byte{2, 0, 0, 0, 0, 2}}
}
func ipv4Frame(src [4]byte, protocol uint8, destination [4]byte, port uint16) []byte {
	b := make([]byte, 14+20+20)
	header.Ethernet(b).Encode(&header.EthernetFields{SrcAddr: tcpip.LinkAddress("\x02\x00\x00\x00\x00\x02"), DstAddr: tcpip.LinkAddress("\x02\x00\x00\x00\x00\x01"), Type: header.IPv4ProtocolNumber})
	header.IPv4(b[14:]).Encode(&header.IPv4Fields{TotalLength: 40, Protocol: protocol, SrcAddr: tcpip.AddrFrom4(src), DstAddr: tcpip.AddrFrom4(destination)})
	if protocol == 17 {
		header.UDP(b[34:]).Encode(&header.UDPFields{SrcPort: 10000, DstPort: port, Length: 20})
	}
	return b
}
func TestPacketAdmission(t *testing.T) {
	n := testNetwork()
	valid := ipv4Frame([4]byte{192, 0, 2, 2}, 6, [4]byte{198, 51, 100, 7}, 8443)
	if !n.accepts(valid, 0) {
		t.Fatal("valid all-port TCP rejected")
	}
	spoof := append([]byte(nil), valid...)
	spoof[6] = 4
	ipv6 := append([]byte(nil), valid...)
	ipv6[12] = 0x86
	ipv6[13] = 0xdd
	vlan := append([]byte(nil), valid...)
	vlan[12] = 0x81
	vlan[13] = 0
	for name, b := range map[string][]byte{
		"short": {1, 2, 3}, "oversized": make([]byte, 1515), "mac": spoof, "ipv6": ipv6, "vlan": vlan,
		"ip":        ipv4Frame([4]byte{192, 0, 2, 3}, 6, [4]byte{198, 51, 100, 7}, 443),
		"other_udp": ipv4Frame([4]byte{192, 0, 2, 2}, 17, [4]byte{192, 0, 2, 1}, 9000),
		"other_dns": ipv4Frame([4]byte{192, 0, 2, 2}, 17, [4]byte{198, 51, 100, 53}, 53),
		"icmp":      ipv4Frame([4]byte{192, 0, 2, 2}, 1, [4]byte{198, 51, 100, 7}, 0),
	} {
		t.Run(name, func(t *testing.T) {
			if n.accepts(b, 0) {
				t.Fatal("unsafe frame admitted")
			}
		})
	}
	if n.accepts(valid, syscall.MSG_TRUNC) {
		t.Fatal("truncated datagram admitted")
	}
	if !n.accepts(ipv4Frame([4]byte{192, 0, 2, 2}, 17, [4]byte{192, 0, 2, 1}, 53), 0) {
		t.Fatal("controlled DNS rejected")
	}
}

func TestARPSourceAndGatewayAreExact(t *testing.T) {
	n := testNetwork()
	b := make([]byte, header.EthernetMinimumSize+header.ARPSize)
	header.Ethernet(b).Encode(&header.EthernetFields{SrcAddr: tcpip.LinkAddress(n.mac[:]), DstAddr: header.EthernetBroadcastAddress, Type: header.ARPProtocolNumber})
	a := header.ARP(b[14:])
	a.SetIPv4OverEthernet()
	a.SetOp(header.ARPRequest)
	copy(a.HardwareAddressSender(), n.mac[:])
	copy(a.ProtocolAddressSender(), []byte{192, 0, 2, 2})
	copy(a.ProtocolAddressTarget(), []byte{192, 0, 2, 1})
	if !n.accepts(b, 0) {
		t.Fatal("valid gateway ARP rejected")
	}
	for _, offset := range []int{14 + 8, 14 + 14, 14 + 24} {
		forged := append([]byte(nil), b...)
		forged[offset] ^= 1
		if n.accepts(forged, 0) {
			t.Fatalf("forged ARP field at %d accepted", offset)
		}
	}
}

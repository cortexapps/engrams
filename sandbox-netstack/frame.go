package main

import (
	"bytes"
	"errors"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"net"
	"net/netip"
	"syscall"
)

const mtu = 1500

var gatewayMAC = []byte{2, 0, 0, 0, 0, 1}

type config struct {
	Version uint32 `json:"version"`
	Guest   string `json:"guest"`
	Gateway string `json:"gateway"`
	MAC     string `json:"mac"`
}
type network struct {
	guest, gateway netip.Addr
	mac            [6]byte
}

func parseNetwork(c config) (network, error) {
	var n network
	var err error
	n.guest, err = netip.ParseAddr(c.Guest)
	if err != nil {
		return n, err
	}
	n.gateway, err = netip.ParseAddr(c.Gateway)
	if err != nil {
		return n, err
	}
	mac, err := net.ParseMAC(c.MAC)
	if err != nil || len(mac) != 6 || mac[0]&1 != 0 || bytes.Equal(mac, gatewayMAC) || bytes.Equal(mac, make([]byte, 6)) || c.Version != 1 || !n.guest.Is4() || !n.gateway.Is4() || !n.guest.IsGlobalUnicast() || !n.gateway.IsGlobalUnicast() || n.guest == n.gateway {
		return n, errors.New("invalid network shape")
	}
	copy(n.mac[:], mac)
	return n, nil
}
func (n network) accepts(frame []byte, flags int) bool {
	if flags&(syscall.MSG_TRUNC|syscall.MSG_CTRUNC) != 0 || len(frame) < header.EthernetMinimumSize || len(frame) > mtu+header.EthernetMinimumSize {
		return false
	}
	eth := header.Ethernet(frame)
	if !bytes.Equal([]byte(eth.SourceAddress()), n.mac[:]) {
		return false
	}
	body := frame[header.EthernetMinimumSize:]
	switch eth.Type() {
	case header.IPv4ProtocolNumber:
		if !bytes.Equal([]byte(eth.DestinationAddress()), gatewayMAC) || len(body) < header.IPv4MinimumSize {
			return false
		}
		ip := header.IPv4(body)
		if !ip.IsValid(len(body)) || ip.SourceAddress().As4() != n.guest.As4() || int(ip.TotalLength()) != len(body) {
			return false
		}
		switch ip.Protocol() {
		case 6:
			return true // TCP reassembly belongs to gVisor.
		case 17:
			// DNS messages are bounded; fragmented UDP is not an accepted transport.
			if ip.FragmentOffset() != 0 || ip.Flags()&header.IPv4FlagMoreFragments != 0 || ip.DestinationAddress().As4() != n.gateway.As4() || len(ip.Payload()) < header.UDPMinimumSize {
				return false
			}
			u := header.UDP(ip.Payload())
			return u.DestinationPort() == 53 && int(u.Length()) == len(ip.Payload())
		default:
			return false
		}
	case header.ARPProtocolNumber:
		if len(body) < header.ARPSize {
			return false
		}
		a := header.ARP(body)
		guest, gateway := n.guest.As4(), n.gateway.As4()
		if !a.IsValid() || !bytes.Equal(a.HardwareAddressSender(), n.mac[:]) || !bytes.Equal(a.ProtocolAddressSender(), guest[:]) || !bytes.Equal(a.ProtocolAddressTarget(), gateway[:]) {
			return false
		}
		if a.Op() == header.ARPRequest {
			return bytes.Equal([]byte(eth.DestinationAddress()), gatewayMAC) || eth.DestinationAddress() == header.EthernetBroadcastAddress
		}
		return a.Op() == header.ARPReply && bytes.Equal([]byte(eth.DestinationAddress()), gatewayMAC) && bytes.Equal(a.HardwareAddressTarget(), gatewayMAC)
	default:
		return false
	}
}

package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"golang.org/x/net/dns/dnsmessage"
	"gvisor.dev/gvisor/pkg/buffer"
	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/adapters/gonet"
	"gvisor.dev/gvisor/pkg/tcpip/header"
	"gvisor.dev/gvisor/pkg/tcpip/link/channel"
	"gvisor.dev/gvisor/pkg/tcpip/link/ethernet"
	"gvisor.dev/gvisor/pkg/tcpip/network/arp"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
	"gvisor.dev/gvisor/pkg/tcpip/stack"
	"gvisor.dev/gvisor/pkg/tcpip/transport/tcp"
	"gvisor.dev/gvisor/pkg/tcpip/transport/udp"
	"io"
	"net"
	"os/exec"
	"path/filepath"
	"sync"
	"syscall"
	"testing"
	"time"
)

func datagramPair(t *testing.T) (*net.UnixConn, *net.UnixConn) {
	t.Helper()
	fd, e := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	if e != nil {
		t.Fatal(e)
	}
	a, e := ownedUnix(fd[0])
	if e != nil {
		t.Fatal(e)
	}
	b, e := ownedUnix(fd[1])
	if e != nil {
		t.Fatal(e)
	}
	return a, b
}
func TestParentLossClosesClosedHelper(t *testing.T) {
	packet, guest := datagramPair(t)
	defer guest.Close()
	parent, control, e := socketPair()
	if e != nil {
		t.Fatal(e)
	}
	defer parent.Close()
	proxy, private, e := socketPair()
	if e != nil {
		t.Fatal(e)
	}
	defer proxy.Close()
	done := make(chan error, 1)
	go func() { done <- run(packet, control, private) }()
	if e = writeFrame(parent, config{1, "192.0.2.2", "192.0.2.1", "02:00:00:00:00:02"}); e != nil {
		t.Fatal(e)
	}
	parent.SetReadDeadline(time.Now().Add(2 * time.Second))
	var r reply
	if e = readFrame(parent, &r); e != nil || r.State != "closed" {
		t.Fatalf("closed readiness: %v %v", r, e)
	}
	parent.Close()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("parent loss left helper alive")
	}
	proxy.SetReadDeadline(time.Now().Add(time.Second))
	var b [1]byte
	if n, _ := proxy.Read(b[:]); n != 0 {
		t.Fatal("private data after parent loss")
	}
}

// The fixture uses only inherited Unix streams. It is a transport peer, not a
// replacement for the Rust policy tests in engram-egress-proxyd.
func receiveFD(c *net.UnixConn) (*net.UnixConn, error) {
	var carrier [1]byte
	control := make([]byte, syscall.CmsgSpace(4))
	n, o, flags, _, e := c.ReadMsgUnix(carrier[:], control)
	if e != nil {
		return nil, e
	}
	if n != 1 || carrier[0] != 0xED || flags != 0 {
		return nil, fmt.Errorf("bad descriptor carrier")
	}
	messages, e := syscall.ParseSocketControlMessage(control[:o])
	if e != nil {
		return nil, e
	}
	fds, e := syscall.ParseUnixRights(&messages[0])
	if e != nil {
		return nil, e
	}
	if len(fds) != 1 {
		return nil, fmt.Errorf("bad descriptor count")
	}
	return ownedUnix(fds[0])
}
func TestNetstackAllTCPAndControlledDNS(t *testing.T) {
	packet, guest := datagramPair(t)
	parent, control, e := socketPair()
	if e != nil {
		t.Fatal(e)
	}
	proxy, private, e := socketPair()
	if e != nil {
		t.Fatal(e)
	}
	done := make(chan error, 1)
	go func() { done <- run(packet, control, private) }()
	t.Cleanup(func() {
		parent.Close()
		proxy.Close()
		guest.Close()
		select {
		case <-done:
		case <-time.After(3 * time.Second):
			t.Error("helper did not join")
		}
	})
	events := make(chan operation, 16)
	fixtureDone := make(chan struct{})
	go func() {
		defer close(fixtureDone)
		defer proxy.Close()
		var flows sync.WaitGroup
		defer flows.Wait()
		for {
			var r request
			if readFrame(proxy, &r) != nil {
				return
			}
			state := "accepted"
			if r.Operation.Kind == "bind" {
				state = "ready"
			} else if r.Operation.Kind == "close" {
				state = "closed"
			} else {
				c, e := receiveFD(proxy)
				if e != nil {
					return
				}
				events <- r.Operation
				flows.Go(func() {
					defer c.Close()
					c.SetDeadline(time.Now().Add(2 * time.Second))
					if r.Operation.Kind == "tcp" {
						_, _ = io.Copy(c, c)
						return
					}
					var length [2]byte
					if _, e := io.ReadFull(c, length[:]); e != nil {
						return
					}
					data := make([]byte, binary.BigEndian.Uint16(length[:]))
					if _, e := io.ReadFull(c, data); e != nil {
						return
					}
					var q dnsmessage.Message
					if q.Unpack(data) != nil {
						return
					}
					q.Response = true
					q.RecursionAvailable = true
					q.Answers = []dnsmessage.Resource{{Header: dnsmessage.ResourceHeader{Name: q.Questions[0].Name, Type: dnsmessage.TypeA, Class: dnsmessage.ClassINET, TTL: 1}, Body: &dnsmessage.AResource{A: [4]byte{198, 51, 100, 7}}}}
					out, e := q.Pack()
					if e != nil {
						return
					}
					binary.BigEndian.PutUint16(length[:], uint16(len(out)))
					_, _ = c.Write(append(length[:], out...))
				})
			}
			if writeFrame(proxy, reply{1, r.Sequence, r.Epoch, state}) != nil {
				return
			}
			if state == "closed" {
				return
			}
		}
	}()
	t.Cleanup(func() {
		proxy.Close()
		select {
		case <-fixtureDone:
		case <-time.After(3 * time.Second):
			t.Error("fixture did not join")
		}
	})
	if e = writeFrame(parent, config{1, "192.0.2.2", "192.0.2.1", "02:00:00:00:00:02"}); e != nil {
		t.Fatal(e)
	}
	var response reply
	if e = readFrame(parent, &response); e != nil {
		t.Fatal(e)
	}
	// Queue a valid TCP SYN while closed. The helper must discard it before OPEN.
	closedSYN := ipv4Frame([4]byte{192, 0, 2, 2}, 6, [4]byte{198, 51, 100, 7}, 8443)
	ip := header.IPv4(closedSYN[14:])
	ip.SetTTL(64)
	ip.SetChecksum(^ip.CalculateChecksum())
	segment := header.TCP(closedSYN[34:])
	segment.Encode(&header.TCPFields{SrcPort: 12345, DstPort: 8443, SeqNum: 1, DataOffset: 20, Flags: header.TCPFlagSyn, WindowSize: 65535})
	segment.SetChecksum(^segment.CalculateChecksum(header.PseudoHeaderChecksum(tcp.ProtocolNumber, ip.SourceAddress(), ip.DestinationAddress(), 20)))
	for range 8 {
		guest.Write(closedSYN)
	}
	if e = writeFrame(parent, command{1, "open", 7}); e != nil {
		t.Fatal(e)
	}
	if e = readFrame(parent, &response); e != nil || response.State != "ready" {
		t.Fatalf("open: %v %v", response, e)
	}
	select {
	case op := <-events:
		t.Fatalf("closed traffic replayed: %v", op)
	default:
	}
	guest.SetReadDeadline(time.Now().Add(40 * time.Millisecond))
	var unexpected [1514]byte
	if size, e := guest.Read(unexpected[:]); e == nil {
		t.Fatalf("closed SYN replay produced %d output bytes", size)
	}
	guest.SetReadDeadline(time.Time{})
	s, stopGuest := guestStack(t, guest)
	defer stopGuest()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	for _, port := range []uint16{443, 8443, 80, 53} {
		c, e := gonet.DialContextTCP(ctx, s, tcpip.FullAddress{NIC: 1, Addr: tcpip.AddrFrom4([4]byte{198, 51, 100, 7}), Port: port}, ipv4.ProtocolNumber)
		if e != nil {
			t.Fatalf("TCP %d: %v", port, e)
		}
		c.SetDeadline(time.Now().Add(2 * time.Second))
		sentinel := []byte("session=forged")
		if _, e = c.Write(sentinel); e != nil {
			t.Fatal(e)
		}
		got := make([]byte, len(sentinel))
		_, e = io.ReadFull(c, got)
		c.Close()
		if e != nil || !bytes.Equal(got, sentinel) {
			t.Fatalf("TCP echo: %q %v", got, e)
		}
		select {
		case op := <-events:
			if op.Kind != "tcp" || op.Port != port || op.Destination != "198.51.100.7" {
				t.Fatalf("tuple: %v", op)
			}
		case <-time.After(2 * time.Second):
			t.Fatalf("no handoff for completed TCP echo on port %d", port)
		}
	}
	q := dnsmessage.Message{Header: dnsmessage.Header{ID: 19, RecursionDesired: true}, Questions: []dnsmessage.Question{{Name: dnsmessage.MustNewName("fixture.example."), Type: dnsmessage.TypeA, Class: dnsmessage.ClassINET}}}
	query, _ := q.Pack()
	for _, transport := range []string{"udp", "tcp"} {
		address := tcpip.FullAddress{NIC: 1, Addr: tcpip.AddrFrom4([4]byte{192, 0, 2, 1}), Port: 53}
		var c net.Conn
		if transport == "udp" {
			c, e = gonet.DialUDP(s, nil, &address, ipv4.ProtocolNumber)
		} else {
			c, e = gonet.DialContextTCP(ctx, s, address, ipv4.ProtocolNumber)
		}
		if e != nil {
			t.Fatal(e)
		}
		c.SetDeadline(time.Now().Add(2 * time.Second))
		out := query
		if transport == "tcp" {
			out = append([]byte{byte(len(query) >> 8), byte(len(query))}, query...)
		}
		if _, e = c.Write(out); e != nil {
			t.Fatal(e)
		}
		data := make([]byte, 4096)
		var count int
		if transport == "tcp" {
			var size [2]byte
			_, e = io.ReadFull(c, size[:])
			if e == nil {
				count = int(binary.BigEndian.Uint16(size[:]))
				_, e = io.ReadFull(c, data[:count])
			}
		} else {
			count, e = c.Read(data)
		}
		c.Close()
		if e != nil {
			t.Fatalf("DNS %s: %v", transport, e)
		}
		var answer dnsmessage.Message
		if e = answer.Unpack(data[:count]); e != nil || len(answer.Answers) != 1 {
			t.Fatalf("DNS %s: %v %v", transport, answer, e)
		}
		select {
		case op := <-events:
			if op.Kind != "dns" || op.Destination != "192.0.2.1" {
				t.Fatalf("DNS tuple: %v", op)
			}
		case <-time.After(2 * time.Second):
			t.Fatal("no DNS handoff")
		}
	}
	if e = writeFrame(parent, command{1, "close", 7}); e != nil {
		t.Fatal(e)
	}
	if e = readFrame(parent, &response); e != nil || response.State != "closed" {
		t.Fatalf("close: %v %v", response, e)
	}
}
func guestStack(t *testing.T, wire *net.UnixConn) (*stack.Stack, func()) {
	t.Helper()
	ep := channel.New(64, 1500, tcpip.LinkAddress("\x02\x00\x00\x00\x00\x02"))
	s := stack.New(stack.Options{NetworkProtocols: []stack.NetworkProtocolFactory{ipv4.NewProtocol, arp.NewProtocol}, TransportProtocols: []stack.TransportProtocolFactory{tcp.NewProtocol, udp.NewProtocol}})
	if e := s.CreateNIC(1, ethernet.New(ep)); e != nil {
		t.Fatal(e)
	}
	if e := s.AddProtocolAddress(1, tcpip.ProtocolAddress{Protocol: ipv4.ProtocolNumber, AddressWithPrefix: tcpip.AddressWithPrefix{Address: tcpip.AddrFrom4([4]byte{192, 0, 2, 2}), PrefixLen: 24}}, stack.AddressProperties{}); e != nil {
		t.Fatal(e)
	}
	s.SetRouteTable([]tcpip.Route{{Destination: header.IPv4EmptySubnet, Gateway: tcpip.AddrFrom4([4]byte{192, 0, 2, 1}), NIC: 1}})
	ctx, cancel := context.WithCancel(context.Background())
	var work sync.WaitGroup
	work.Go(func() {
		for {
			p := ep.ReadContext(ctx)
			if p == nil {
				return
			}
			v := p.ToView()
			_, e := wire.Write(v.AsSlice())
			v.Release()
			p.DecRef()
			if e != nil {
				return
			}
		}
	})
	work.Go(func() {
		b := make([]byte, 1515)
		for {
			n, e := wire.Read(b)
			if e != nil {
				return
			}
			p := stack.NewPacketBuffer(stack.PacketBufferOptions{Payload: buffer.MakeWithData(append([]byte(nil), b[:n]...))})
			ep.InjectInbound(header.Ethernet(b[:n]).Type(), p)
			p.DecRef()
		}
	})
	return s, func() {
		cancel()
		wire.Close()
		ep.Close()
		s.Close()
		work.Wait()
		s.Wait()
	}
}

func TestInheritedProcessLosesParentAndProxy(t *testing.T) {
	binary := filepath.Join(t.TempDir(), "netstack")
	build := exec.Command("go", "build", "-mod=readonly", "-trimpath", "-o", binary, ".")
	if output, e := build.CombinedOutput(); e != nil {
		t.Fatalf("build helper: %v %s", e, output)
	}
	for _, lost := range []string{"parent", "proxy"} {
		t.Run(lost, func(t *testing.T) {
			packet, guest := datagramPair(t)
			defer guest.Close()
			parent, control, e := socketPair()
			if e != nil {
				t.Fatal(e)
			}
			defer parent.Close()
			proxy, private, e := socketPair()
			if e != nil {
				t.Fatal(e)
			}
			defer proxy.Close()
			cmd := exec.Command(binary)
			cmd.Env = []string{}
			f0, e := packet.File()
			if e != nil {
				t.Fatal(e)
			}
			f1, e := control.File()
			if e != nil {
				t.Fatal(e)
			}
			f2, e := private.File()
			if e != nil {
				t.Fatal(e)
			}
			cmd.Stdin = f0
			cmd.Stdout = f1
			cmd.Stderr = f2
			e = cmd.Start()
			f0.Close()
			f1.Close()
			f2.Close()
			packet.Close()
			control.Close()
			private.Close()
			if e != nil {
				t.Fatal(e)
			}
			waited := make(chan error, 1)
			go func() { waited <- cmd.Wait() }()
			t.Cleanup(func() {
				cmd.Process.Kill()
				select {
				case <-waited:
				case <-time.After(2 * time.Second):
					t.Error("child was not reaped")
				}
			})
			if e = writeFrame(parent, config{1, "192.0.2.2", "192.0.2.1", "02:00:00:00:00:02"}); e != nil {
				t.Fatal(e)
			}
			parent.SetReadDeadline(time.Now().Add(2 * time.Second))
			var ready reply
			if e = readFrame(parent, &ready); e != nil || ready.State != "closed" {
				t.Fatalf("child ready: %v %v", ready, e)
			}
			if lost == "parent" {
				parent.Close()
			} else {
				proxy.Close()
			}
			select {
			case e = <-waited:
				if e == nil {
					t.Fatal("lost channel exit must be nonzero")
				}
				waited <- e
			case <-time.After(2 * time.Second):
				t.Fatal("lost channel left helper running")
			}
		})
	}
}

func TestCloseJoinsHalfOpenTCP(t *testing.T) {
	packet, guest := datagramPair(t)
	defer guest.Close()
	parent, control, e := socketPair()
	if e != nil {
		t.Fatal(e)
	}
	defer parent.Close()
	proxy, private, e := socketPair()
	if e != nil {
		t.Fatal(e)
	}
	defer proxy.Close()
	done := make(chan error, 1)
	go func() { done <- run(packet, control, private) }()
	t.Cleanup(func() {
		parent.Close()
		proxy.Close()
		select {
		case <-done:
		case <-time.After(3 * time.Second):
			t.Error("half-open helper did not join")
		}
	})
	fixture := make(chan struct{})
	go func() {
		defer close(fixture)
		defer proxy.Close()
		for {
			var r request
			if readFrame(proxy, &r) != nil {
				return
			}
			state := "ready"
			if r.Operation.Kind == "close" {
				state = "closed"
			}
			if r.Operation.Kind != "bind" && r.Operation.Kind != "close" {
				return
			}
			if writeFrame(proxy, reply{1, r.Sequence, r.Epoch, state}) != nil || state == "closed" {
				return
			}
		}
	}()
	t.Cleanup(func() {
		proxy.Close()
		select {
		case <-fixture:
		case <-time.After(time.Second):
			t.Error("fixture not joined")
		}
	})
	if e = writeFrame(parent, config{1, "192.0.2.2", "192.0.2.1", "02:00:00:00:00:02"}); e != nil {
		t.Fatal(e)
	}
	var response reply
	if readFrame(parent, &response) != nil {
		t.Fatal("no readiness")
	}
	if e = writeFrame(parent, command{1, "open", 7}); e != nil {
		t.Fatal(e)
	}
	if readFrame(parent, &response) != nil {
		t.Fatal("no open ack")
	}
	syn := ipv4Frame([4]byte{192, 0, 2, 2}, 6, [4]byte{198, 51, 100, 7}, 8443)
	ip := header.IPv4(syn[14:])
	ip.SetTTL(64)
	ip.SetChecksum(^ip.CalculateChecksum())
	segment := header.TCP(syn[34:])
	segment.Encode(&header.TCPFields{SrcPort: 12345, DstPort: 8443, SeqNum: 1, DataOffset: 20, Flags: header.TCPFlagSyn, WindowSize: 65535})
	segment.SetChecksum(^segment.CalculateChecksum(header.PseudoHeaderChecksum(tcp.ProtocolNumber, ip.SourceAddress(), ip.DestinationAddress(), 20)))
	if _, e = guest.Write(syn); e != nil {
		t.Fatal(e)
	}
	guest.SetReadDeadline(time.Now().Add(time.Second))
	var packetOut [1514]byte
	if _, e = guest.Read(packetOut[:]); e != nil {
		t.Fatalf("no handshake or neighbor request: %v", e)
	}
	if e = writeFrame(parent, command{1, "close", 7}); e != nil {
		t.Fatal(e)
	}
	parent.SetReadDeadline(time.Now().Add(3 * time.Second))
	if e = readFrame(parent, &response); e != nil || response.State != "closed" {
		t.Fatalf("close did not join half-open TCP: %v %v", response, e)
	}
}

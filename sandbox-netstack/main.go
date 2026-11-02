// The only inherited descriptors are 0=packet, 1=parent control, 2=proxy
// capability. None is a log. No arguments, environment config, or listener.
package main

import (
	"context"
	"encoding/binary"
	"errors"
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
	"gvisor.dev/gvisor/pkg/waiter"
	"io"
	"net"
	"os"
	"sync"
	"syscall"
	"time"
)

const maxFlows = 64
const maxDNS = 8

type gate struct {
	mu          sync.Mutex
	open, dead  bool
	active, dns int
	conns       map[net.Conn]bool
	work        sync.WaitGroup
}

func (g *gate) enter(dns bool) bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	if !g.open || g.dead || g.active >= maxFlows || (dns && g.dns >= maxDNS) {
		return false
	}
	g.active++
	if dns {
		g.dns++
	}
	g.work.Add(1)
	return true
}
func (g *gate) leave(dns bool) {
	g.mu.Lock()
	g.active--
	if dns {
		g.dns--
	}
	g.mu.Unlock()
	g.work.Done()
}
func (g *gate) track(c net.Conn) bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	if !g.open || g.dead {
		c.Close()
		return false
	}
	g.conns[c] = true
	return true
}
func (g *gate) untrack(c net.Conn) { c.Close(); g.mu.Lock(); delete(g.conns, c); g.mu.Unlock() }
func (g *gate) close() {
	g.mu.Lock()
	defer g.mu.Unlock()
	g.open = false
	g.dead = true
	for c := range g.conns {
		c.Close()
	}
}
func main() {
	if len(os.Args) != 1 {
		os.Exit(1)
	}
	packet, e := ownedUnix(0)
	if e != nil {
		os.Exit(1)
	}
	control, e := ownedUnix(1)
	if e != nil {
		packet.Close()
		os.Exit(1)
	}
	private, e := ownedUnix(2)
	if e != nil {
		packet.Close()
		control.Close()
		os.Exit(1)
	}
	if run(packet, control, private) != nil {
		os.Exit(1)
	}
}
func run(packet, control, private *net.UnixConn) error {
	defer packet.Close()
	defer control.Close()
	defer private.Close()
	var cfg config
	control.SetReadDeadline(time.Now().Add(handoffTimeout))
	if err := readFrame(control, &cfg); err != nil {
		return err
	}
	n, err := parseNetwork(cfg)
	if err != nil {
		return err
	}
	// Socket types are part of the attachment contract.
	for conn, want := range map[*net.UnixConn]int{packet: syscall.SOCK_DGRAM, control: syscall.SOCK_STREAM, private: syscall.SOCK_STREAM} {
		raw, e := conn.SyscallConn()
		if e != nil {
			return e
		}
		var actual int
		var inner error
		if e = raw.Control(func(fd uintptr) { actual, inner = syscall.GetsockoptInt(int(fd), syscall.SOL_SOCKET, syscall.SO_TYPE) }); e != nil {
			return e
		}
		if inner != nil || actual != want {
			return errors.New("wrong private descriptor type")
		}
	}
	g := &gate{conns: make(map[net.Conn]bool)}
	stop := make(chan struct{})
	var once sync.Once
	fail := func() {
		once.Do(func() {
			g.close()
			close(stop)
			packet.Close()
			control.Close()
			// Interrupt a pending handoff write without sending cleanup EOF.
			private.SetWriteDeadline(time.Now())
		})
	}
	defer fail()
	i := &ingress{conn: private, replies: make(chan reply), stop: stop, fail: fail}
	var channels sync.WaitGroup
	defer func() {
		fail()
		// This defer runs after stack, packet, and flow cleanup below. EOF on
		// this reliable stream is the proxy's cleanup acknowledgement.
		private.SetReadDeadline(time.Now())
		channels.Wait()
		private.Close()
	}()
	channels.Go(i.read)
	commands := make(chan command)
	channels.Go(func() {
		for {
			var c command
			if readFrame(control, &c) != nil {
				fail()
				return
			}
			select {
			case commands <- c:
			case <-stop:
				return
			}
		}
	})
	ep := channel.New(64, mtu, tcpip.LinkAddress(gatewayMAC))
	s := stack.New(stack.Options{NetworkProtocols: []stack.NetworkProtocolFactory{ipv4.NewProtocol, arp.NewProtocol}, TransportProtocols: []stack.TransportProtocolFactory{tcp.NewProtocol, udp.NewProtocol}})
	defer func() { s.Close(); s.Wait() }()
	if e := s.CreateNIC(1, ethernet.New(ep)); e != nil {
		return errors.New(e.String())
	}
	address := tcpip.AddrFrom4(n.gateway.As4())
	if e := s.AddProtocolAddress(1, tcpip.ProtocolAddress{Protocol: ipv4.ProtocolNumber, AddressWithPrefix: address.WithPrefix()}, stack.AddressProperties{}); e != nil {
		return errors.New(e.String())
	}
	if e := s.SetPromiscuousMode(1, true); e != nil {
		return errors.New(e.String())
	}
	if e := s.SetSpoofing(1, true); e != nil {
		return errors.New(e.String())
	}
	s.SetRouteTable([]tcpip.Route{{Destination: header.IPv4EmptySubnet, NIC: 1}})
	// Conservative per-flow buffers and bounded SYN retries. There is no load
	// tuning knob at this boundary.
	for _, option := range []tcpip.SettableTransportProtocolOption{
		&tcpip.TCPSendBufferSizeRangeOption{Min: 4096, Default: 65536, Max: 65536},
		&tcpip.TCPReceiveBufferSizeRangeOption{Min: 4096, Default: 65536, Max: 65536},
	} {
		if e := s.SetTransportProtocolOption(tcp.ProtocolNumber, option); e != nil {
			return errors.New(e.String())
		}
	}
	retries := tcpip.TCPSynRetriesOption(1)
	if e := s.SetTransportProtocolOption(tcp.ProtocolNumber, &retries); e != nil {
		return errors.New(e.String())
	}
	exchangeDNS := func(data []byte) ([]byte, error) {
		var m dnsmessage.Message
		if len(data) == 0 || len(data) > dnsLimit {
			return nil, errors.New("DNS size")
		}
		if e := m.Unpack(data); e != nil {
			return nil, e
		}
		if m.Response || len(m.Questions) != 1 {
			return nil, errors.New("DNS query shape")
		}
		c, e := i.flow("dns", n.gateway.String(), 53)
		if e != nil {
			return nil, e
		}
		if !g.track(c) {
			return nil, errors.New("closed")
		}
		defer g.untrack(c)
		c.SetDeadline(time.Now().Add(10 * time.Second))
		var size [2]byte
		binary.BigEndian.PutUint16(size[:], uint16(len(data)))
		if _, e = c.Write(append(size[:], data...)); e != nil {
			return nil, e
		}
		if _, e = io.ReadFull(c, size[:]); e != nil {
			return nil, e
		}
		length := int(binary.BigEndian.Uint16(size[:]))
		if length == 0 || length > dnsLimit {
			return nil, errors.New("DNS reply size")
		}
		out := make([]byte, length)
		if _, e = io.ReadFull(c, out); e != nil {
			return nil, e
		}
		var response dnsmessage.Message
		if e = response.Unpack(out); e != nil {
			return nil, e
		}
		if !response.Response || response.ID != m.ID || len(response.Questions) != 1 || response.Questions[0] != m.Questions[0] {
			return nil, errors.New("DNS reply mismatch")
		}
		return out, nil
	}
	forward := tcp.NewForwarder(s, 65536, 16, func(r *tcp.ForwarderRequest) {
		id := r.ID()
		dns := id.LocalAddress == address && id.LocalPort == 53
		if id.RemoteAddress != tcpip.AddrFrom4(n.guest.As4()) || !g.enter(dns) {
			r.Complete(true)
			return
		}
		defer g.leave(dns)
		var queue waiter.Queue
		endpoint, tcpErr := r.CreateEndpoint(&queue)
		r.Complete(false)
		if tcpErr != nil {
			return
		}
		c := gonet.NewTCPConn(&queue, endpoint)
		if !g.track(c) {
			return
		}
		defer g.untrack(c)
		c.SetDeadline(time.Now().Add(5 * time.Minute))
		if dns {
			c.SetDeadline(time.Now().Add(10 * time.Second))
			var size [2]byte
			if _, e := io.ReadFull(c, size[:]); e != nil {
				return
			}
			length := int(binary.BigEndian.Uint16(size[:]))
			if length == 0 || length > dnsLimit {
				return
			}
			data := make([]byte, length)
			if _, e := io.ReadFull(c, data); e != nil {
				return
			}
			out, e := exchangeDNS(data)
			if e != nil {
				return
			}
			binary.BigEndian.PutUint16(size[:], uint16(len(out)))
			// Close sets the gate under this same lock; no response can start after it.
			g.mu.Lock()
			if g.open && !g.dead {
				c.SetWriteDeadline(time.Now().Add(time.Second))
				_, _ = c.Write(append(size[:], out...))
			}
			g.mu.Unlock()
			return
		}
		upstream, e := i.flow("tcp", id.LocalAddress.String(), id.LocalPort)
		if e != nil {
			return
		}
		if !g.track(upstream) {
			return
		}
		defer g.untrack(upstream)
		upstream.SetDeadline(time.Now().Add(5 * time.Minute))
		finished := make(chan struct{})
		go func() { _, _ = io.Copy(upstream, c); upstream.CloseWrite(); close(finished) }()
		_, _ = io.Copy(c, upstream)
		c.Close()
		upstream.Close()
		<-finished
	})
	s.SetTransportProtocolHandler(tcp.ProtocolNumber, forward.HandlePacket)
	udpConn, e := gonet.DialUDP(s, &tcpip.FullAddress{NIC: 1, Addr: address, Port: 53}, nil, ipv4.ProtocolNumber)
	if e != nil {
		return e
	}
	var workers sync.WaitGroup
	workers.Go(func() {
		b := make([]byte, dnsLimit+1)
		for {
			length, peer, e := udpConn.ReadFrom(b)
			if e != nil {
				return
			}
			if length > dnsLimit || !g.enter(true) {
				continue
			}
			data := append([]byte(nil), b[:length]...)
			go func() {
				defer g.leave(true)
				out, e := exchangeDNS(data)
				if e != nil {
					return
				}
				g.mu.Lock()
				defer g.mu.Unlock()
				if g.open && !g.dead {
					udpConn.SetWriteDeadline(time.Now().Add(time.Second))
					_, _ = udpConn.WriteTo(out, peer)
				}
			}()
		}
	})
	workers.Go(func() {
		for {
			p := ep.ReadContext(context.Background())
			if p == nil {
				return
			}
			v := p.ToView()
			var writeErr error
			g.mu.Lock()
			if g.open && !g.dead {
				packet.SetWriteDeadline(time.Now().Add(250 * time.Millisecond))
				_, writeErr = packet.Write(v.AsSlice())
			}
			g.mu.Unlock()
			v.Release()
			p.DecRef()
			if writeErr != nil {
				fail()
				return
			}
		}
	})
	activate := make(chan chan error)
	workers.Go(func() {
		b := make([]byte, mtu+15)
		for {
			select {
			case result := <-activate:
				// The receiver owns this drain. No pre-OPEN datagram remains queued, and
				// an input flood fails activation instead of extending an unbounded drain.
				raw, e := packet.SyscallConn()
				if e == nil {
					controlErr := raw.Control(func(fd uintptr) {
						for count := 0; count < 512; count++ {
							_, _, _, _, x := syscall.Recvmsg(int(fd), b, nil, syscall.MSG_DONTWAIT)
							if x == syscall.EAGAIN || x == syscall.EWOULDBLOCK {
								return
							}
							if x != nil {
								e = x
								return
							}
						}
						e = errors.New("closed packet queue did not drain")
					})
					if controlErr != nil {
						e = controlErr
					}
				}
				g.mu.Lock()
				if e == nil && !g.dead {
					g.open = true
				} else if e == nil {
					e = errors.New("closed")
				}
				g.mu.Unlock()
				result <- e
			default:
			}
			packet.SetReadDeadline(time.Now().Add(25 * time.Millisecond))
			length, _, flags, _, e := packet.ReadMsgUnix(b, nil)
			if e != nil {
				if timeout, ok := e.(net.Error); ok && timeout.Timeout() {
					continue
				}
				g.mu.Lock()
				dead := g.dead
				g.mu.Unlock()
				if !dead {
					fail()
				}
				return
			}
			g.mu.Lock()
			open := g.open && !g.dead
			g.mu.Unlock()
			if !open || !n.accepts(b[:length], flags) {
				continue
			}
			p := stack.NewPacketBuffer(stack.PacketBufferOptions{Payload: buffer.MakeWithData(append([]byte(nil), b[:length]...))})
			ep.InjectInbound(header.Ethernet(b[:length]).Type(), p)
			p.DecRef()
		}
	})
	var cleanOnce sync.Once
	cleanup := func() {
		cleanOnce.Do(func() {
			g.close()
			packet.Close()
			udpConn.Close()
			ep.Close()
			s.Close()
			workers.Wait()
			g.work.Wait()
			s.Close()
			s.Wait()
		})
	}
	defer func() { cleanup(); fail() }()
	if e := writeFrame(control, reply{1, 0, 0, "closed"}); e != nil {
		return e
	}
	opened := false
	for {
		select {
		case <-stop:
			return errors.New("private channel closed")
		case c := <-commands:
			if c.Version != 1 || c.Epoch == 0 || c.Epoch > 9007199254740991 {
				return errors.New("invalid command")
			}
			switch c.Kind {
			case "open":
				if opened {
					return errors.New("allocation already used")
				}
				opened = true
				i.epoch = c.Epoch
				if e := i.call(operation{Kind: "bind"}, nil); e != nil {
					return e
				}
				result := make(chan error, 1)
				select {
				case activate <- result:
				case <-stop:
					return errors.New("closed")
				}
				select {
				case e := <-result:
					if e != nil {
						return e
					}
				case <-stop:
					return errors.New("closed")
				}
				if e := writeFrame(control, reply{1, 1, c.Epoch, "ready"}); e != nil {
					return e
				}
			case "close":
				if !opened || c.Epoch != i.epoch {
					return errors.New("wrong epoch")
				}
				cleanup()
				if e := i.call(operation{Kind: "close"}, nil); e != nil {
					return e
				}
				if e := writeFrame(control, reply{1, 2, c.Epoch, "closed"}); e != nil {
					return e
				}
				return nil
			default:
				return errors.New("unknown command")
			}
		}
	}
}

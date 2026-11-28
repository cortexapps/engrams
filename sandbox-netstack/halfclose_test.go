package main

import (
	"context"
	"io"
	"net"
	"testing"
	"time"

	"gvisor.dev/gvisor/pkg/tcpip"
	"gvisor.dev/gvisor/pkg/tcpip/adapters/gonet"
	"gvisor.dev/gvisor/pkg/tcpip/network/ipv4"
)

// Use run's TCP forwarder, packet path, and descriptor handoff for each flow.
func halfCloseFlow(t *testing.T) (*gonet.TCPConn, *net.UnixConn, *net.UnixConn) {
	t.Helper()
	packet, wire := datagramPair(t)
	parent, control, err := socketPair()
	if err != nil {
		t.Fatal(err)
	}
	proxy, private, err := socketPair()
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- run(packet, control, private) }()
	flows := make(chan *net.UnixConn, 1)
	fixtureDone := make(chan struct{})
	go func() {
		defer close(fixtureDone)
		for {
			var r request
			if readFrame(proxy, &r) != nil {
				return
			}
			state := "accepted"
			switch r.Operation.Kind {
			case "bind":
				state = "ready"
			case "close":
				state = "closed"
			case "tcp":
				c, err := receiveFD(proxy)
				if err != nil {
					return
				}
				flows <- c
			default:
				return
			}
			if writeFrame(proxy, reply{1, r.Sequence, r.Epoch, state}) != nil || state == "closed" {
				return
			}
		}
	}()
	t.Cleanup(func() {
		parent.Close()
		proxy.Close()
		wire.Close()
		select {
		case <-done:
		case <-time.After(3 * time.Second):
			t.Error("helper did not join")
		}
		select {
		case <-fixtureDone:
		case <-time.After(time.Second):
			t.Error("proxy fixture did not join")
		}
		select {
		case c := <-flows:
			c.Close()
		default:
		}
	})
	parent.SetDeadline(time.Now().Add(2 * time.Second))
	if err = writeFrame(parent, config{1, "192.0.2.2", "192.0.2.1", "02:00:00:00:00:02"}); err != nil {
		t.Fatal(err)
	}
	var response reply
	if err = readFrame(parent, &response); err != nil || response.State != "closed" {
		t.Fatalf("readiness: %v %v", response, err)
	}
	if err = writeFrame(parent, command{1, "open", 7}); err != nil {
		t.Fatal(err)
	}
	if err = readFrame(parent, &response); err != nil || response.State != "ready" {
		t.Fatalf("open: %v %v", response, err)
	}
	s, stopGuest := guestStack(t, wire)
	t.Cleanup(stopGuest)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	guest, err := gonet.DialContextTCP(ctx, s, tcpip.FullAddress{NIC: 1, Addr: tcpip.AddrFrom4([4]byte{198, 51, 100, 7}), Port: 8443}, ipv4.ProtocolNumber)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { guest.Close() })
	guest.SetDeadline(time.Now().Add(5 * time.Second))
	select {
	case upstream := <-flows:
		t.Cleanup(func() { upstream.Close() })
		upstream.SetDeadline(time.Now().Add(5 * time.Second))
		return guest, upstream, parent
	case <-ctx.Done():
		t.Fatal("no TCP descriptor handoff")
		return nil, nil, nil
	}
}

func TestUpstreamHalfClosePreservesGuestUpload(t *testing.T) {
	guest, upstream, _ := halfCloseFlow(t)
	if _, err := guest.Write([]byte("request-start")); err != nil {
		t.Fatal(err)
	}
	initial := make([]byte, len("request-start"))
	if _, err := io.ReadFull(upstream, initial); err != nil || string(initial) != "request-start" {
		t.Fatalf("initial upload: %q %v", initial, err)
	}
	if _, err := upstream.Write([]byte("response-complete")); err != nil {
		t.Fatal(err)
	}
	if err := upstream.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	if response, err := io.ReadAll(guest); err != nil || string(response) != "response-complete" {
		t.Fatalf("response and EOF: %q %v", response, err)
	}
	if _, err := guest.Write([]byte("distinct-trailing-upload")); err != nil {
		t.Fatalf("upload after response EOF: %v", err)
	}
	if err := guest.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	if trailing, err := io.ReadAll(upstream); err != nil || string(trailing) != "distinct-trailing-upload" {
		t.Fatalf("trailing upload and EOF: %q %v", trailing, err)
	}
}

func TestGuestHalfClosePreservesUpstreamResponse(t *testing.T) {
	guest, upstream, _ := halfCloseFlow(t)
	if _, err := guest.Write([]byte("complete-request")); err != nil {
		t.Fatal(err)
	}
	if _, err := upstream.Write([]byte("response-start")); err != nil {
		t.Fatal(err)
	}
	initial := make([]byte, len("response-start"))
	if _, err := io.ReadFull(guest, initial); err != nil || string(initial) != "response-start" {
		t.Fatalf("initial response: %q %v", initial, err)
	}
	if err := guest.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	if request, err := io.ReadAll(upstream); err != nil || string(request) != "complete-request" {
		t.Fatalf("request and EOF: %q %v", request, err)
	}
	if _, err := upstream.Write([]byte("distinct-trailing-response")); err != nil {
		t.Fatal(err)
	}
	if err := upstream.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	if trailing, err := io.ReadAll(guest); err != nil || string(trailing) != "distinct-trailing-response" {
		t.Fatalf("trailing response and EOF: %q %v", trailing, err)
	}
}

func TestCloseJoinsActiveHalfClosedTCP(t *testing.T) {
	guest, upstream, parent := halfCloseFlow(t)
	if err := upstream.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	if response, err := io.ReadAll(guest); err != nil || len(response) != 0 {
		t.Fatalf("response EOF: %q %v", response, err)
	}
	if _, err := guest.Write([]byte("still-active")); err != nil {
		t.Fatal(err)
	}
	active := make([]byte, len("still-active"))
	if _, err := io.ReadFull(upstream, active); err != nil || string(active) != "still-active" {
		t.Fatalf("active upload before close: %q %v", active, err)
	}
	if err := writeFrame(parent, command{1, "close", 7}); err != nil {
		t.Fatal(err)
	}
	var response reply
	if err := readFrame(parent, &response); err != nil || response.State != "closed" {
		t.Fatalf("close did not join active flow: %v %v", response, err)
	}
	if data, err := io.ReadAll(upstream); err != nil || len(data) != 0 {
		t.Fatalf("revoked upstream: %q %v", data, err)
	}
}

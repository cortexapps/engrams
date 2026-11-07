package main

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"sync"
	"syscall"
	"time"
)

const headerLimit = 2048
const dnsLimit = 4096
const handoffTimeout = 5 * time.Second

type operation struct {
	Kind        string `json:"kind"`
	Destination string `json:"destination,omitempty"`
	Port        uint16 `json:"port,omitempty"`
}
type request struct {
	Version   uint32    `json:"version"`
	Sequence  uint64    `json:"sequence"`
	Epoch     uint64    `json:"epoch"`
	Operation operation `json:"operation"`
}
type reply struct {
	Version  uint32 `json:"version"`
	Sequence uint64 `json:"sequence"`
	Epoch    uint64 `json:"epoch"`
	State    string `json:"state"`
}
type command struct {
	Version uint32 `json:"version"`
	Kind    string `json:"kind"`
	Epoch   uint64 `json:"epoch"`
}

func readFrame(c *net.UnixConn, v any) error {
	var head [4]byte
	// Idle reliable channels stay live; partial frames have a fixed budget.
	if _, err := io.ReadFull(c, head[:1]); err != nil {
		return err
	}
	if err := c.SetReadDeadline(time.Now().Add(handoffTimeout)); err != nil {
		return err
	}
	defer c.SetReadDeadline(time.Time{})
	if _, err := io.ReadFull(c, head[1:]); err != nil {
		return err
	}
	length := binary.BigEndian.Uint32(head[:])
	if length == 0 || length > headerLimit {
		return errors.New("invalid private header")
	}
	data := make([]byte, length)
	if _, err := io.ReadFull(c, data); err != nil {
		return err
	}
	d := json.NewDecoder(bytes.NewReader(data))
	d.DisallowUnknownFields()
	if err := d.Decode(v); err != nil {
		return err
	}
	if d.Decode(new(any)) != io.EOF {
		return errors.New("extra private data")
	}
	return nil
}
func writeFrame(c *net.UnixConn, v any) error {
	body, err := json.Marshal(v)
	if err != nil {
		return err
	}
	if len(body) > headerLimit {
		return errors.New("private header too large")
	}
	data := make([]byte, 4, len(body)+4)
	binary.BigEndian.PutUint32(data, uint32(len(body)))
	data = append(data, body...)
	if err = c.SetWriteDeadline(time.Now().Add(handoffTimeout)); err != nil {
		return err
	}
	defer c.SetWriteDeadline(time.Time{})
	for len(data) > 0 {
		n, e := c.Write(data)
		if e != nil {
			return e
		}
		data = data[n:]
	}
	return nil
}
func socketPair() (*net.UnixConn, *net.UnixConn, error) {
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		return nil, nil, err
	}
	syscall.CloseOnExec(fds[0])
	syscall.CloseOnExec(fds[1])
	a, e := ownedUnix(fds[0])
	if e != nil {
		syscall.Close(fds[1])
		return nil, nil, e
	}
	b, e := ownedUnix(fds[1])
	if e != nil {
		a.Close()
		return nil, nil, e
	}
	return a, b, nil
}
func ownedUnix(fd int) (*net.UnixConn, error) {
	f := os.NewFile(uintptr(fd), "private-network")
	defer f.Close()
	c, err := net.FileConn(f)
	if err != nil {
		return nil, err
	}
	unix, ok := c.(*net.UnixConn)
	if !ok {
		c.Close()
		return nil, errors.New("expected Unix descriptor")
	}
	return unix, nil
}
func sendFD(c *net.UnixConn, fd *net.UnixConn) error {
	raw, err := fd.SyscallConn()
	if err != nil {
		return err
	}
	var writeErr error
	err = raw.Control(func(value uintptr) {
		_, _, writeErr = c.WriteMsgUnix([]byte{0xED}, syscall.UnixRights(int(value)), nil)
	})
	if err != nil {
		return err
	}
	return writeErr
}

type ingress struct {
	conn            *net.UnixConn
	mu              sync.Mutex
	sequence, epoch uint64
	replies         chan reply
	stop            <-chan struct{}
	fail            func()
}

func (i *ingress) read() {
	for {
		var r reply
		if readFrame(i.conn, &r) != nil {
			i.fail()
			return
		}
		select {
		case i.replies <- r:
		case <-i.stop:
			return
		}
		if r.State == "closed" && r.Sequence > 0 {
			return
		}
	}
}
func (i *ingress) call(op operation, fd *net.UnixConn) error {
	i.mu.Lock()
	defer i.mu.Unlock()
	select {
	case <-i.stop:
		return errors.New("closed")
	default:
	}
	i.sequence++
	if err := writeFrame(i.conn, request{1, i.sequence, i.epoch, op}); err != nil {
		i.fail()
		return err
	}
	if fd != nil {
		i.conn.SetWriteDeadline(time.Now().Add(handoffTimeout))
		defer i.conn.SetWriteDeadline(time.Time{})
		if err := sendFD(i.conn, fd); err != nil {
			i.fail()
			return err
		}
	}
	timer := time.NewTimer(handoffTimeout)
	defer timer.Stop()
	select {
	case r := <-i.replies:
		want := "accepted"
		if op.Kind == "bind" {
			want = "ready"
		}
		if op.Kind == "close" {
			want = "closed"
		}
		if r.Version == 1 && r.Sequence == i.sequence && r.Epoch == i.epoch && r.State == want {
			return nil
		}
	case <-timer.C:
	case <-i.stop:
	}
	i.fail()
	return errors.New("private handoff failed")
}
func (i *ingress) flow(kind, destination string, port uint16) (*net.UnixConn, error) {
	a, b, err := socketPair()
	if err != nil {
		i.fail()
		return nil, err
	}
	defer b.Close()
	if err = i.call(operation{kind, destination, port}, b); err != nil {
		a.Close()
		return nil, err
	}
	return a, nil
}

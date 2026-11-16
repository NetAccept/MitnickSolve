package server

import (
	"context"
	"fmt"
	"net"
	"strings"
	"time"
)

// netRule is one entry in the deployment egress allowlist. A rule is either a
// CIDR block or a domain suffix; "example.com" also covers subdomains.
type netRule struct {
	cidr   *net.IPNet
	domain string
}

func parseNetAllow(values []string) ([]netRule, error) {
	rules := make([]netRule, 0, len(values))
	for _, raw := range values {
		for _, value := range strings.Split(raw, ",") {
			value = strings.TrimSpace(value)
			if value == "" {
				continue
			}
			if _, network, err := net.ParseCIDR(value); err == nil {
				rules = append(rules, netRule{cidr: network})
				continue
			}
			domain := strings.TrimPrefix(strings.ToLower(strings.TrimSpace(value)), ".")
			if domain == "" || strings.ContainsAny(domain, " /\\") {
				return nil, fmt.Errorf("invalid net allow entry %q: use a CIDR or domain suffix", value)
			}
			rules = append(rules, netRule{domain: domain})
		}
	}
	return rules, nil
}

// ValidateNetAllow rejects malformed --net-allow values at startup.
func ValidateNetAllow(values []string) error {
	_, err := parseNetAllow(values)
	return err
}

func (s *Server) authorizeNetHost(host string) error {
	if len(s.opts.NetAllow) == 0 {
		return nil
	}
	if s.netRuleErr != nil {
		return fmt.Errorf("invalid net allowlist: %w", s.netRuleErr)
	}
	host = strings.TrimSpace(host)
	if host == "" {
		return fmt.Errorf("net allowlist: empty host")
	}
	if ip := net.ParseIP(host); ip != nil {
		if s.ipAllowed(ip) {
			return nil
		}
		return fmt.Errorf("host %s is outside the configured net allowlist", host)
	}
	domain := strings.TrimSuffix(strings.ToLower(host), ".")
	for _, rule := range s.netRules {
		if rule.domain != "" &&
			(domain == rule.domain || strings.HasSuffix(domain, "."+rule.domain)) {
			return nil
		}
	}
	// A hostname may still be allowed by a CIDR if it resolves into it. Check
	// all returned addresses and fail closed if any lookup errors.
	a4, err := func() ([]net.IP, error) {
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		addresses, err := net.DefaultResolver.LookupIPAddr(ctx, host)
		if err != nil {
			return nil, err
		}
		ips := make([]net.IP, 0, len(addresses))
		for _, address := range addresses {
			ips = append(ips, address.IP)
		}
		return ips, nil
	}()
	if err != nil {
		return fmt.Errorf("net allowlist: cannot resolve %s: %w", host, err)
	}
	for _, ip := range a4 {
		if s.ipAllowed(ip) {
			return nil
		}
	}
	return fmt.Errorf("host %s is outside the configured net allowlist", host)
}

func (s *Server) ipAllowed(ip net.IP) bool {
	for _, rule := range s.netRules {
		if rule.cidr != nil && rule.cidr.Contains(ip) {
			return true
		}
	}
	return false
}

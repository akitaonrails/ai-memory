Name:           ai-memory
# The release workflow replaces this placeholder from Cargo.toml.
Version:        0.3.2
Release:        1
Summary:        Local-first long-term memory MCP server for AI coding agents
License:        MIT
URL:            https://github.com/akitaonrails/ai-memory
Source0:        https://github.com/akitaonrails/ai-memory/releases/download/v%{version}/ai-memory-linux-%{_arch}.tar.gz
BuildArch:      %{_arch}
%global debug_package %{nil}
Requires:       systemd
Requires:       ca-certificates
Requires:       glibc
%global _unitdir /usr/lib/systemd/system
%global _userunitdir /usr/lib/systemd/user

%description
Cross-session memory for AI coding agents, with a local MCP server and
systemd service.

%prep
%setup -q -c

%install
install -Dpm0755 ai-memory %{buildroot}%{_bindir}/ai-memory
install -Dpm0644 README.md %{buildroot}%{_datadir}/doc/ai-memory/README.md
install -Dpm0644 docs/install.md %{buildroot}%{_datadir}/doc/ai-memory/install.md
install -Dpm0644 LICENSE %{buildroot}%{_licensedir}/ai-memory/LICENSE
cp -a hooks %{buildroot}%{_datadir}/ai-memory/
install -Dpm0644 packaging/systemd/ai-memory.service %{buildroot}%{_unitdir}/ai-memory.service
install -Dpm0644 packaging/systemd/ai-memory-user.service %{buildroot}%{_userunitdir}/ai-memory.service
install -Dpm0644 packaging/sysusers/ai-memory.conf %{buildroot}%{_prefix}/lib/sysusers.d/ai-memory.conf
install -Dpm0644 packaging/tmpfiles/ai-memory.conf %{buildroot}%{_prefix}/lib/tmpfiles.d/ai-memory.conf
install -Dpm0644 crates/ai-memory-cli/templates/config.default.toml %{buildroot}%{_sysconfdir}/ai-memory/config.toml
install -Dpm0640 packaging/env/ai-memory.env %{buildroot}%{_sysconfdir}/ai-memory/env

%files
%config(noreplace) %{_sysconfdir}/ai-memory/config.toml
%config(noreplace) %{_sysconfdir}/ai-memory/env
%{_bindir}/ai-memory
%{_datadir}/ai-memory/
%{_datadir}/doc/ai-memory/
%{_licensedir}/ai-memory/LICENSE
%{_unitdir}/ai-memory.service
%{_userunitdir}/ai-memory.service
%{_prefix}/lib/sysusers.d/ai-memory.conf
%{_prefix}/lib/tmpfiles.d/ai-memory.conf

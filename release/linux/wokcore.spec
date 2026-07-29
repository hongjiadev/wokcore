Name: wokcore
Version: %{wokcore_version}
Release: 1
Summary: Independent local Provider gateway
License: MIT OR Apache-2.0
BuildArch: %{wokcore_arch}

%description
WokCore independent local Provider gateway.

%install
install -D -m 0755 "%{wokcore_executable}" %{buildroot}/usr/bin/wokcore
install -D -m 0644 "%{license_apache}" %{buildroot}/usr/share/doc/wokcore/LICENSE-APACHE
install -D -m 0644 "%{license_mit}" %{buildroot}/usr/share/doc/wokcore/LICENSE-MIT
install -D -m 0644 "%{notice}" %{buildroot}/usr/share/doc/wokcore/NOTICE.md
install -D -m 0644 "%{readme}" %{buildroot}/usr/share/doc/wokcore/README.md

%files
/usr/bin/wokcore
/usr/share/doc/wokcore/LICENSE-APACHE
/usr/share/doc/wokcore/LICENSE-MIT
/usr/share/doc/wokcore/NOTICE.md
/usr/share/doc/wokcore/README.md

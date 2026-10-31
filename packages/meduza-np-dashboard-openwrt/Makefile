# OpenWrt package (LuCI feed layout): copy this directory into feeds/luci/applications/ or build it with the SDK as
# package/meduza-np-dashboard-openwrt. luci.mk installs htdocs/ into /www and root/ into /; its postinst clears the
# LuCI caches and HUPs rpcd so the new ACL and ubus object are picked up. No resident process is added.

include $(TOPDIR)/rules.mk

PKG_NAME:=meduza-np-dashboard-openwrt
PKG_VERSION:=0.1.0
PKG_LICENSE:=Apache-2.0

LUCI_TITLE:=Meduza NP node dashboard (LuCI)
LUCI_DESCRIPTION:=Status, features, instances, maintenance and updates of the Meduza NP planner through rpcd and the meduza-np CLI.
LUCI_DEPENDS:=+meduza-np-cli +rpcd +jsonfilter +luci-base
LUCI_PKGARCH:=all

include $(TOPDIR)/feeds/luci/luci.mk

# call BuildPackage - OpenWrt buildroot signature

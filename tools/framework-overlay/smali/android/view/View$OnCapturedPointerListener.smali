# ECLIPSE PATCH 2026-06-13: AOSP android.view.View.OnCapturedPointerListener (API 26). ATL's installed View
# omits the pointer-capture API; Roblox (ActivityNativeMain.d1) references this interface and calls
# View.setOnCapturedPointerListener. Assembled into the overlay's smali-View dex by patch-framework.sh
# (the View.smali side — field + setter — is added at build time by anchored insertion into the
# baksmali'd installed View, so it stays faithful to whatever View the installed jar actually ships).
# Modeled byte-for-byte on the installed View$OnClickListener.smali (same accessFlags 0x609 = public
# static interface abstract).
.class public interface abstract Landroid/view/View$OnCapturedPointerListener;
.super Ljava/lang/Object;


# annotations
.annotation system Ldalvik/annotation/EnclosingClass;
    value = Landroid/view/View;
.end annotation

.annotation system Ldalvik/annotation/InnerClass;
    accessFlags = 0x609
    name = "OnCapturedPointerListener"
.end annotation


# virtual methods
.method public abstract onCapturedPointer(Landroid/view/View;Landroid/view/MotionEvent;)Z
.end method

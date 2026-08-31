.class public Landroid/net/LinkAddress;
.super Ljava/lang/Object;

.method public constructor <init>()V
    .registers 1

    invoke-direct {p0}, Ljava/lang/Object;-><init>()V

    return-void
.end method

.method public getAddress()Ljava/net/InetAddress;
    .registers 2

    const/4 v0, 0x0

    return-object v0
.end method

.method public getPrefixLength()I
    .registers 2

    const/4 v0, 0x0

    return v0
.end method

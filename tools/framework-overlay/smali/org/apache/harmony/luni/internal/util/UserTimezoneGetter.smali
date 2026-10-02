.class final Lorg/apache/harmony/luni/internal/util/UserTimezoneGetter;
.super Lorg/apache/harmony/luni/internal/util/TimezoneGetter;

.method constructor <init>()V
    .registers 1

    invoke-direct {p0}, Lorg/apache/harmony/luni/internal/util/TimezoneGetter;-><init>()V

    return-void
.end method

.method public getId()Ljava/lang/String;
    .registers 2

    const-string v0, "user.timezone"

    invoke-static {v0}, Ljava/lang/System;->getProperty(Ljava/lang/String;)Ljava/lang/String;

    move-result-object v0

    return-object v0
.end method

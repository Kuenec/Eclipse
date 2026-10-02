.class final Landroid/app/PermissionResultDelivery;
.super Ljava/lang/Object;
.implements Ljava/lang/Runnable;

.field private final activity:Landroid/app/Activity;

.field private final requestCode:I

.field private final permissions:[Ljava/lang/String;

.field private final grantResults:[I

.method private constructor <init>(Landroid/app/Activity;I[Ljava/lang/String;[I)V
    .registers 5

    invoke-direct {p0}, Ljava/lang/Object;-><init>()V

    iput-object p1, p0, Landroid/app/PermissionResultDelivery;->activity:Landroid/app/Activity;

    iput p2, p0, Landroid/app/PermissionResultDelivery;->requestCode:I

    iput-object p3, p0, Landroid/app/PermissionResultDelivery;->permissions:[Ljava/lang/String;

    iput-object p4, p0, Landroid/app/PermissionResultDelivery;->grantResults:[I

    return-void
.end method

.method static post(Landroid/app/Activity;[Ljava/lang/String;I)V
    .registers 7

    if-gez p2, :request_code_valid

    new-instance v0, Ljava/lang/IllegalArgumentException;

    const-string v1, "requestCode should be >= 0"

    invoke-direct {v0, v1}, Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V

    throw v0

    :request_code_valid
    array-length v0, p1

    new-array v1, v0, [I

    const/4 v2, 0x0

    :next_permission
    if-ge v2, v0, :permissions_checked

    aget-object v3, p1, v2

    invoke-virtual {p0, v3}, Landroid/app/Activity;->checkSelfPermission(Ljava/lang/String;)I

    move-result v3

    aput v3, v1, v2

    add-int/lit8 v2, v2, 0x1

    goto :next_permission

    :permissions_checked
    new-instance v0, Landroid/os/Handler;

    invoke-static {}, Landroid/os/Looper;->getMainLooper()Landroid/os/Looper;

    move-result-object v2

    invoke-direct {v0, v2}, Landroid/os/Handler;-><init>(Landroid/os/Looper;)V

    new-instance v2, Landroid/app/PermissionResultDelivery;

    invoke-direct {v2, p0, p2, p1, v1}, Landroid/app/PermissionResultDelivery;-><init>(Landroid/app/Activity;I[Ljava/lang/String;[I)V

    invoke-virtual {v0, v2}, Landroid/os/Handler;->post(Ljava/lang/Runnable;)Z

    return-void
.end method

.method public run()V
    .registers 5

    iget-object v0, p0, Landroid/app/PermissionResultDelivery;->activity:Landroid/app/Activity;

    iget v1, p0, Landroid/app/PermissionResultDelivery;->requestCode:I

    iget-object v2, p0, Landroid/app/PermissionResultDelivery;->permissions:[Ljava/lang/String;

    iget-object v3, p0, Landroid/app/PermissionResultDelivery;->grantResults:[I

    invoke-virtual {v0, v1, v2, v3}, Landroid/app/Activity;->onRequestPermissionsResult(I[Ljava/lang/String;[I)V

    return-void
.end method
